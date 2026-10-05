//! Networking: finds Intel Ethernet controllers, brings them up through the
//! C++ e1000 driver and runs a smoltcp TCP/IP stack on the first one from a
//! kernel thread. DHCP configures the address; `ping` sends ICMP echoes.
//! On igb/igc cards the thread sleeps until the card's MSI-X interrupt says
//! a frame arrived (or smoltcp's next timer is due) instead of polling every
//! tick.
//!
//! Programs get UDP and TCP sockets as handles (`UserSocket`). The stack
//! (interface, device and every socket) lives in `STACK`; a system call
//! locks it to queue data or take what arrived, and wakes the network
//! thread to send. A call that has to wait sleeps on `SOCKET_WAITERS`,
//! which the network thread wakes after every poll.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, icmp, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, IpEndpoint, Ipv4Cidr};

use crate::dhi::{self, NetInfo};
use crate::sched::Thread;
use crate::sync::IrqMutex;
use crate::{apic, arch, console, pci, sched};

/// Which C++ driver runs a card.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    E1000,
    Igc,
}

/// A card as the drivers know it: driver plus that driver's NIC id.
#[derive(Clone, Copy)]
pub struct Port {
    pub driver: Driver,
    pub id: i32,
}

impl Port {
    fn send(self, frame: &[u8]) -> bool {
        let (p, n) = (frame.as_ptr(), frame.len() as u32);
        let rc = unsafe {
            match self.driver {
                Driver::E1000 => dhi::aero_e1000_send(self.id, p, n),
                Driver::Igc => dhi::aero_igc_send(self.id, p, n),
            }
        };
        rc == 0
    }

    fn recv(self, buf: &mut [u8]) -> i32 {
        let (p, n) = (buf.as_mut_ptr(), buf.len() as u32);
        unsafe {
            match self.driver {
                Driver::E1000 => dhi::aero_e1000_recv(self.id, p, n),
                Driver::Igc => dhi::aero_igc_recv(self.id, p, n),
            }
        }
    }

    /// Current link speed in Mb/s, read from the hardware; None if the link is down.
    pub fn link(self) -> Option<u32> {
        let mut speed = 0;
        let up = unsafe {
            match self.driver {
                Driver::E1000 => dhi::aero_e1000_link(self.id, &mut speed),
                Driver::Igc => dhi::aero_igc_link(self.id, &mut speed),
            }
        };
        (up == 1).then_some(speed)
    }
}

pub struct Nic {
    pub port: Port,
    pub driver_name: &'static str,
    pub name: String,
    pub location: String,
    pub pci_id: (u16, u16),
    pub info: NetInfo,
    pub pci: pci::Device,
}

/// What DHCP gave the interface.
#[derive(Clone, Copy)]
pub struct Lease {
    pub address: Ipv4Cidr,
    pub router: Option<Ipv4Addr>,
    pub dns: Option<Ipv4Addr>,
}

pub static NICS: IrqMutex<Vec<Nic>> = IrqMutex::new(Vec::new());
pub static LEASE: IrqMutex<Option<Lease>> = IrqMutex::new(None);
pub static RX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static TX_PACKETS: AtomicU64 = AtomicU64::new(0);
pub static RX_ERRORS: AtomicU64 = AtomicU64::new(0);

/// A ping the shell asked for; the network thread runs it and sets `done`.
struct PingJob {
    target: Ipv4Addr,
    count: u16,
    done: bool,
}

static PING: IrqMutex<Option<PingJob>> = IrqMutex::new(None);

/// Wakes the network thread: the card's interrupt, or a new ping job.
static WAKE: sched::Event = sched::Event::new();
pub static IRQS: AtomicU64 = AtomicU64::new(0);

fn on_irq() {
    IRQS.fetch_add(1, Ordering::Relaxed);
    WAKE.signal();
}

/// The longest the network thread sleeps when nothing happens: smoltcp's
/// timers (DHCP renewals, TCP retransmits) and pings are checked at least
/// this often.
const MAX_SLEEP_TICKS: u64 = 10;

/// Brings up every supported Intel NIC. Returns how many came up.
pub fn probe() -> usize {
    let mut found = 0;
    for dev in pci::devices().iter().filter(|d| d.class == 0x02 && d.subclass == 0x00 && d.vendor == 0x8086) {
        let at = alloc::format!("{:02x}:{:02x}.{}", dev.bus, dev.dev, dev.func);
        let (driver, driver_name) = if unsafe { dhi::aero_e1000_supports(dev.device) } != 0 {
            (Driver::E1000, "e1000")
        } else if unsafe { dhi::aero_igc_supports(dev.device) } != 0 {
            (Driver::Igc, "igb/igc")
        } else {
            console::print_colored(console::YELLOW, format_args!(
                "[WARN] Intel Ethernet 8086:{:04x} at {}: no driver for this model yet\n", dev.device, at));
            continue;
        };
        let Some(bar0) = dev.bar(0) else { continue };
        dev.enable_mmio_and_dma();
        let mut info = NetInfo::default();
        let id = unsafe {
            match driver {
                Driver::E1000 => dhi::aero_e1000_init(&dhi::OPS, bar0, &mut info),
                Driver::Igc => dhi::aero_igc_init(&dhi::OPS, bar0, dev.device, &mut info),
            }
        };
        if id < 0 {
            console::print_colored(console::YELLOW, format_args!("[WARN] Intel Ethernet at {}: init failed ({})\n", at, id));
            continue;
        }
        let mut nics = NICS.lock();
        let name = alloc::format!("eth{}", nics.len());
        nics.push(Nic { port: Port { driver, id }, driver_name, name, location: at, pci_id: (dev.vendor, dev.device), info, pci: *dev });
        found += 1;
    }
    found
}

pub fn mac_string(mac: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

fn now() -> Instant {
    Instant::from_micros(apic::micros() as i64)
}

// ------------------------------------------------------ smoltcp device glue

struct Device {
    nic: Port,
    rx: [u8; 2048],
}

struct RxToken<'a>(&'a [u8]);
struct TxToken(Port);

impl phy::RxToken for RxToken<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        RX_PACKETS.fetch_add(1, Ordering::Relaxed);
        f(self.0)
    }
}

impl phy::TxToken for TxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut frame = [0u8; 1514];
        let r = f(&mut frame[..len]);
        if self.0.send(&frame[..len]) {
            TX_PACKETS.fetch_add(1, Ordering::Relaxed);
        }
        r
    }
}

impl phy::Device for Device {
    type RxToken<'a> = RxToken<'a>;
    type TxToken<'a> = TxToken;

    fn receive(&mut self, _: Instant) -> Option<(RxToken<'_>, TxToken)> {
        loop {
            let n = self.nic.recv(&mut self.rx);
            match n {
                0 => return None,
                n if n < 0 => {
                    RX_ERRORS.fetch_add(1, Ordering::Relaxed);
                }
                n => return Some((RxToken(&self.rx[..n as usize]), TxToken(self.nic))),
            }
        }
    }

    fn transmit(&mut self, _: Instant) -> Option<TxToken> {
        Some(TxToken(self.nic))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(32);
        caps
    }
}

// ----------------------------------------------------------- the stack

const PING_IDENT: u16 = 0xAE40;

/// Kernel thread: runs the TCP/IP stack on the first NIC, about every 10 ms.
pub fn net_thread(_: u64) {
    let Some((nic, mac, pci, name)) = NICS.lock().first().map(|n| (n.port, n.info.mac, n.pci, n.name.clone())) else { return };
    // Interrupts aimed at this CPU, where this thread runs. igb/igc only:
    // the e1000 family stays polled.
    let irq = if nic.driver == Driver::Igc {
        crate::msi::attach(&pci, &alloc::format!("{} ({:02x}:{:02x}.{})", name, pci.bus, pci.dev, pci.func), on_irq, crate::percpu::this().index)
    } else {
        None
    };
    match irq {
        Some(kind) => {
            unsafe { dhi::aero_igc_enable_irq(nic.id) };
            console::print_colored(console::DIM, format_args!("       {}: {} interrupts on, receiving at once when frames arrive\n", name, kind));
        }
        None => console::print_colored(console::DIM, format_args!("       {}: polled every tick\n", name)),
    }
    let mut device = Device { nic, rx: [0; 2048] };
    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let iface = Interface::new(config, &mut device, now());

    let mut sockets = SocketSet::new(vec![]);
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let icmp_socket = icmp::Socket::new(
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
    );
    let icmp = sockets.add(icmp_socket);
    sockets.get_mut::<icmp::Socket>(icmp).bind(icmp::Endpoint::Ident(PING_IDENT)).ok();
    *STACK.lock() = Some(Stack { iface, device, sockets, lingering: Vec::new() });

    let mut pinger = Pinger::default();
    let mut pinged_gateway = false;
    loop {
        if irq.is_some() {
            // Clear the causes first: a frame arriving after this interrupts again.
            unsafe { dhi::aero_igc_ack_irq(nic.id) };
        }
        let delay = {
            let mut guard = STACK.lock();
            let st = guard.as_mut().unwrap();
            close_dropped(st);
            st.poll();
            let Stack { iface, sockets, .. } = st;
            dhcp_events(iface, sockets, dhcp, &mut pinged_gateway);
            pinger.step(sockets, icmp, &ChecksumCapabilities::default());
            // Send what the pinger just queued now, not after the sleep.
            st.poll();
            st.iface.poll_delay(now(), &st.sockets)
        };
        wake_socket_waiters();
        if irq.is_some() {
            // Sleep until a frame arrives, a program sends, or smoltcp has
            // a timer due.
            let ticks = delay.map_or(MAX_SLEEP_TICKS, |d| d.total_millis() * apic::TIMER_HZ / 1000);
            WAKE.wait(ticks.clamp(1, MAX_SLEEP_TICKS));
        } else {
            sched::sleep_ticks(1);
        }
    }
}

/// Handles what the DHCP client reports: a new lease configures the
/// interface (and pings the gateway once), a lost one clears it.
fn dhcp_events(iface: &mut Interface, sockets: &mut SocketSet<'static>, dhcp: SocketHandle, pinged_gateway: &mut bool) {
    if let Some(event) = sockets.get_mut::<dhcpv4::Socket>(dhcp).poll() {
        match event {
            dhcpv4::Event::Configured(cfg) => {
                iface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    addrs.push(IpCidr::Ipv4(cfg.address)).ok();
                });
                match cfg.router {
                    Some(r) => {
                        iface.routes_mut().add_default_ipv4_route(r).ok();
                    }
                    None => {
                        iface.routes_mut().remove_default_ipv4_route();
                    }
                }
                let lease = Lease { address: cfg.address, router: cfg.router, dns: cfg.dns_servers.first().copied() };
                crate::kok!("DHCP: eth0 got {}, gateway {}, DNS {}", lease.address,
                    opt(lease.router), opt(lease.dns));
                *LEASE.lock() = Some(lease);
                // Prove the link works both ways: one ping to the gateway.
                if let (Some(r), false) = (cfg.router, *pinged_gateway) {
                    *pinged_gateway = true;
                    *PING.lock() = Some(PingJob { target: r, count: 1, done: false });
                }
            }
            dhcpv4::Event::Deconfigured => {
                iface.update_ip_addrs(|addrs| addrs.clear());
                iface.routes_mut().remove_default_ipv4_route();
                // The socket also reports this once at start, before any lease.
                if LEASE.lock().take().is_some() {
                    console::print_colored(console::YELLOW, format_args!("[WARN] DHCP: eth0 lost its address\n"));
                }
            }
        }
    }
}

// ------------------------------------------------------- shared stack

/// The interface, the card and every socket, kernel and user. Whoever
/// holds `STACK` may queue data or take what arrived; only the network
/// thread polls (sends and receives frames).
struct Stack {
    iface: Interface,
    device: Device,
    sockets: SocketSet<'static>,
    /// Closed TCP sockets finishing their goodbye: handle, give-up time
    /// (`apic::micros`), and whether they were aborted already.
    lingering: Vec<(SocketHandle, u64, bool)>,
}

impl Stack {
    fn poll(&mut self) {
        self.iface.poll(now(), &mut self.device, &mut self.sockets);
    }
}

static STACK: IrqMutex<Option<Stack>> = IrqMutex::new(None);

/// Threads waiting in a socket call; all are woken after each poll and
/// check again.
static SOCKET_WAITERS: IrqMutex<Vec<Arc<Thread>>> = IrqMutex::new(Vec::new());

/// Sockets whose last handle went away: (socket, is TCP). `UserSocket`'s
/// drop only queues here (it may run with scheduler locks held); the
/// network thread closes them.
static CLOSING: IrqMutex<Vec<(SocketHandle, bool)>> = IrqMutex::new(Vec::new());

/// How long a closed TCP connection may take to say goodbye before it is
/// reset.
const LINGER_US: u64 = 2_000_000;

/// Programs' sockets still in the stack (open, or closing).
pub static PROGRAM_SOCKETS: AtomicUsize = AtomicUsize::new(0);

fn wake_socket_waiters() {
    let waiters = core::mem::take(&mut *SOCKET_WAITERS.lock());
    for t in &waiters {
        sched::wake_waiter(t);
    }
}

fn close_dropped(st: &mut Stack) {
    let t = apic::micros();
    for (handle, tcp) in CLOSING.lock().drain(..) {
        if tcp {
            st.sockets.get_mut::<tcp::Socket>(handle).close();
            st.lingering.push((handle, t + LINGER_US, false));
        } else {
            st.sockets.remove(handle);
            PROGRAM_SOCKETS.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let Stack { sockets, lingering, .. } = st;
    lingering.retain_mut(|(handle, give_up, aborted)| {
        let s = sockets.get_mut::<tcp::Socket>(*handle);
        // An aborted socket had a poll to send its reset.
        if *aborted || matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait) {
            sockets.remove(*handle);
            PROGRAM_SOCKETS.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        if t >= *give_up {
            s.abort();
            *aborted = true;
        }
        true
    });
}

/// Next local port for a program's socket (the IANA ephemeral range).
static NEXT_PORT: AtomicU16 = AtomicU16::new(49152);

fn ephemeral_port() -> u16 {
    loop {
        let p = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        if p >= 49152 {
            return p;
        }
        // Wrapped past 65535: start the range again.
        NEXT_PORT.store(49152, Ordering::Relaxed);
    }
}

/// Why a socket call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SockError {
    /// No network card, or no address from DHCP yet.
    NoNetwork,
    /// Refused, reset, or not connected.
    Closed,
    TimedOut,
    Invalid,
    /// The caller's process is exiting.
    Killed,
}

/// A UDP or TCP socket owned by a program (through a handle).
pub struct UserSocket {
    handle: SocketHandle,
    pub tcp: bool,
    /// UDP: where `send` goes and the only sender `recv` accepts.
    remote: IrqMutex<Option<IpEndpoint>>,
    /// TCP: the handshake finished once.
    connected: AtomicBool,
    /// TCP after `listen`: sockets listening on the port, each waiting for
    /// one connection; `accept` hands out one that got its connection and
    /// puts a fresh one in its place.
    backlog: IrqMutex<Vec<SocketHandle>>,
    listening: AtomicBool,
}

impl Drop for UserSocket {
    fn drop(&mut self) {
        let mut closing = CLOSING.lock();
        closing.push((self.handle, self.tcp));
        closing.extend(self.backlog.lock().drain(..).map(|h| (h, true)));
    }
}

/// Connections a listening socket can take before `accept` picks them up.
const BACKLOG: usize = 4;

/// A fresh TCP socket listening on `port`.
fn add_listener(st: &mut Stack, port: u16) -> Result<SocketHandle, SockError> {
    let mut s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_BUFFER]), tcp::SocketBuffer::new(vec![0; TCP_BUFFER]));
    s.listen(port).map_err(|_| SockError::Invalid)?;
    PROGRAM_SOCKETS.fetch_add(1, Ordering::Relaxed);
    Ok(st.sockets.add(s))
}

/// What a program can learn about the network: zero addresses when there
/// is no lease yet.
pub fn info() -> Option<Lease> {
    *LEASE.lock()
}

const TCP_BUFFER: usize = 64 * 1024;
const UDP_PACKETS: usize = 16;
const UDP_BUFFER: usize = 16 * 1024;

/// Runs `attempt` on the stack until it gives an answer, sleeping between
/// polls of the network thread. `deadline` is `apic::micros` time.
fn wait_for<T>(deadline: Option<u64>, mut attempt: impl FnMut(&mut Stack) -> Option<Result<T, SockError>>) -> Result<T, SockError> {
    arch::without_interrupts(|| loop {
        let me = {
            let mut guard = STACK.lock();
            let st = guard.as_mut().ok_or(SockError::NoNetwork)?;
            if let Some(r) = attempt(st) {
                return r;
            }
            if deadline.is_some_and(|d| apic::micros() >= d) {
                return Err(SockError::TimedOut);
            }
            let me = match deadline {
                Some(d) => sched::mark_current_sleeping(d),
                None => sched::mark_current_blocked(),
            };
            // Pushed under STACK: the network thread wakes waiters only
            // after a poll it made holding STACK, so none is missed.
            SOCKET_WAITERS.lock().push(me.clone());
            me
        };
        if sched::killed() {
            sched::unblock_current();
            SOCKET_WAITERS.lock().retain(|t| !Arc::ptr_eq(t, &me));
            return Err(SockError::Killed);
        }
        sched::schedule();
        SOCKET_WAITERS.lock().retain(|t| !Arc::ptr_eq(t, &me));
        if sched::killed() {
            return Err(SockError::Killed);
        }
    })
}

impl UserSocket {
    /// A new socket with a local port of its own.
    pub fn open(tcp: bool) -> Result<Arc<UserSocket>, SockError> {
        let mut guard = STACK.lock();
        let st = guard.as_mut().ok_or(SockError::NoNetwork)?;
        let handle = if tcp {
            let s = tcp::Socket::new(tcp::SocketBuffer::new(vec![0; TCP_BUFFER]), tcp::SocketBuffer::new(vec![0; TCP_BUFFER]));
            st.sockets.add(s)
        } else {
            let mut s = udp::Socket::new(
                udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]),
                udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; UDP_PACKETS], vec![0; UDP_BUFFER]),
            );
            s.bind(ephemeral_port()).map_err(|_| SockError::Invalid)?;
            st.sockets.add(s)
        };
        PROGRAM_SOCKETS.fetch_add(1, Ordering::Relaxed);
        Ok(Arc::new(UserSocket::new(handle, tcp)))
    }

    fn new(handle: SocketHandle, tcp: bool) -> UserSocket {
        UserSocket {
            handle,
            tcp,
            remote: IrqMutex::new(None),
            connected: AtomicBool::new(false),
            backlog: IrqMutex::new(Vec::new()),
            listening: AtomicBool::new(false),
        }
    }

    /// TCP: waits for connections on `port`, `BACKLOG` at a time.
    pub fn listen(&self, port: u16) -> Result<(), SockError> {
        if !self.tcp || port == 0 || self.connected.load(Ordering::Relaxed) || self.remote.lock().is_some() {
            return Err(SockError::Invalid);
        }
        if self.listening.swap(true, Ordering::SeqCst) {
            return Err(SockError::Invalid);
        }
        let mut guard = STACK.lock();
        let st = guard.as_mut().ok_or(SockError::NoNetwork)?;
        let mut backlog = self.backlog.lock();
        for _ in 0..BACKLOG {
            backlog.push(add_listener(st, port)?);
        }
        Ok(())
    }

    /// TCP after `listen`: waits up to `timeout_us` (0 = no limit) for a
    /// connection and returns it as a new socket.
    pub fn accept(&self, timeout_us: u64) -> Result<Arc<UserSocket>, SockError> {
        if !self.listening.load(Ordering::SeqCst) {
            return Err(SockError::Invalid);
        }
        let deadline = (timeout_us != 0).then(|| apic::micros().saturating_add(timeout_us));
        let conn = wait_for(deadline, |st| {
            let mut backlog = self.backlog.lock();
            let i = backlog.iter().position(|&h| {
                matches!(st.sockets.get::<tcp::Socket>(h).state(), tcp::State::Established | tcp::State::CloseWait)
            })?;
            let h = backlog[i];
            let s = st.sockets.get::<tcp::Socket>(h);
            let (port, remote) = (s.listen_endpoint().port, s.remote_endpoint());
            // A fresh listener takes its place in the backlog.
            match add_listener(st, port) {
                Ok(fresh) => backlog[i] = fresh,
                Err(e) => return Some(Err(e)),
            }
            Some(Ok((h, remote)))
        });
        let (h, remote) = conn?;
        let sock = UserSocket::new(h, true);
        *sock.remote.lock() = remote;
        sock.connected.store(true, Ordering::Relaxed);
        Ok(Arc::new(sock))
    }

    /// UDP: sets the peer. TCP: connects, waiting up to `timeout_us`
    /// (0 = no limit) for the handshake.
    pub fn connect(&self, addr: Ipv4Addr, port: u16, timeout_us: u64) -> Result<(), SockError> {
        if port == 0 {
            return Err(SockError::Invalid);
        }
        if LEASE.lock().is_none() {
            return Err(SockError::NoNetwork);
        }
        let remote = IpEndpoint::new(IpAddress::Ipv4(addr), port);
        *self.remote.lock() = Some(remote);
        if !self.tcp {
            return Ok(());
        }
        {
            let mut guard = STACK.lock();
            let st = guard.as_mut().ok_or(SockError::NoNetwork)?;
            let Stack { iface, sockets, .. } = st;
            let s = sockets.get_mut::<tcp::Socket>(self.handle);
            if s.state() != tcp::State::Closed {
                return Err(SockError::Invalid);
            }
            s.connect(iface.context(), remote, ephemeral_port()).map_err(|_| SockError::Invalid)?;
        }
        WAKE.signal();
        let deadline = (timeout_us != 0).then(|| apic::micros().saturating_add(timeout_us));
        let r = wait_for(deadline, |st| {
            let s = st.sockets.get_mut::<tcp::Socket>(self.handle);
            match s.state() {
                tcp::State::Established => Some(Ok(())),
                tcp::State::SynSent | tcp::State::SynReceived => None,
                _ => Some(Err(SockError::Closed)),
            }
        });
        if r.is_ok() {
            self.connected.store(true, Ordering::Relaxed);
        }
        if r == Err(SockError::TimedOut) {
            // Give up on the handshake so the socket can try again.
            if let Some(st) = STACK.lock().as_mut() {
                st.sockets.get_mut::<tcp::Socket>(self.handle).abort();
            }
            WAKE.signal();
        }
        r
    }

    /// Queues `data`; returns how much was taken (TCP may take part of it,
    /// waiting only until some fits). UDP needs `connect` first.
    pub fn send(&self, data: &[u8]) -> Result<usize, SockError> {
        let remote = *self.remote.lock();
        let r = if self.tcp {
            wait_for(None, |st| {
                let s = st.sockets.get_mut::<tcp::Socket>(self.handle);
                if !s.may_send() {
                    return Some(Err(SockError::Closed));
                }
                if !s.can_send() {
                    return None;
                }
                Some(s.send_slice(data).map_err(|_| SockError::Closed))
            })
        } else {
            let remote = remote.ok_or(SockError::Invalid)?;
            wait_for(None, |st| {
                let s = st.sockets.get_mut::<udp::Socket>(self.handle);
                match s.send_slice(data, remote) {
                    Ok(()) => Some(Ok(data.len())),
                    Err(udp::SendError::BufferFull) if data.len() <= UDP_BUFFER => None,
                    Err(_) => Some(Err(SockError::Invalid)),
                }
            })
        };
        if r.is_ok() {
            WAKE.signal();
        }
        r
    }

    /// Takes up to `max` bytes that arrived, waiting up to `timeout_us`
    /// (0 = no limit) for some. TCP returns an empty Vec once the peer has
    /// closed and everything was read. UDP returns one datagram (cut to
    /// `max`).
    pub fn recv(&self, max: usize, timeout_us: u64) -> Result<Vec<u8>, SockError> {
        let deadline = (timeout_us != 0).then(|| apic::micros().saturating_add(timeout_us));
        let remote = *self.remote.lock();
        let mut buf = vec![0u8; max];
        let r = if self.tcp {
            wait_for(deadline, |st| {
                let s = st.sockets.get_mut::<tcp::Socket>(self.handle);
                if s.can_recv() {
                    return Some(s.recv_slice(&mut buf).map_err(|_| SockError::Closed));
                }
                if s.may_recv() || matches!(s.state(), tcp::State::SynSent | tcp::State::SynReceived) {
                    return None;
                }
                // Closed by the peer (or reset) after connecting: end of
                // data. Never connected: an error.
                Some(if self.connected.load(Ordering::Relaxed) { Ok(0) } else { Err(SockError::Closed) })
            })
        } else {
            wait_for(deadline, |st| {
                let s = st.sockets.get_mut::<udp::Socket>(self.handle);
                loop {
                    let Ok((data, meta)) = s.recv() else { return None };
                    if remote.is_some_and(|r| r != meta.endpoint) {
                        continue; // not from our peer
                    }
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    return Some(Ok(n));
                }
            })
        };
        let n = r?;
        if self.tcp && n > 0 {
            // Room in the receive window: let the network thread say so.
            WAKE.signal();
        }
        buf.truncate(n);
        Ok(buf)
    }
}

pub fn opt(a: Option<Ipv4Addr>) -> String {
    a.map_or(String::from("none"), |a| alloc::format!("{}", a))
}

/// Sends one echo per second for the current job and prints the replies.
#[derive(Default)]
struct Pinger {
    seq: u16,
    sent: u16,
    received: u16,
    sent_at: u64,
    sent_us: u64,
    active: bool,
}

impl Pinger {
    fn step(&mut self, sockets: &mut SocketSet, handle: SocketHandle, caps: &ChecksumCapabilities) {
        let job = PING.lock().as_ref().filter(|j| !j.done).map(|j| (j.target, j.count));
        let Some((target, count)) = job else {
            self.active = false;
            return;
        };
        if !self.active {
            *self = Pinger { seq: self.seq, active: true, ..Default::default() };
        }
        let socket = sockets.get_mut::<icmp::Socket>(handle);
        let t = sched::ticks();

        while let Ok((payload, from)) = socket.recv() {
            let Ok(packet) = Icmpv4Packet::new_checked(payload) else { continue };
            if let Ok(Icmpv4Repr::EchoReply { ident: PING_IDENT, seq_no, .. }) = Icmpv4Repr::parse(&packet, caps) {
                self.received += 1;
                let us = apic::micros().saturating_sub(self.sent_us);
                crate::kok!("ping {}: reply from {}, seq {}, time {}.{:03} ms", target, from, seq_no, us / 1000, us % 1000);
            }
        }

        let timeout = apic::TIMER_HZ; // one echo per second, one second to answer
        if self.sent < count && (self.sent == 0 || t >= self.sent_at + timeout) {
            let data = b"AeroForge ping!!";
            let repr = Icmpv4Repr::EchoRequest { ident: PING_IDENT, seq_no: self.seq, data };
            if let Ok(buf) = socket.send(repr.buffer_len(), IpAddress::Ipv4(target)) {
                repr.emit(&mut Icmpv4Packet::new_unchecked(buf), caps);
                self.seq = self.seq.wrapping_add(1);
                self.sent += 1;
                self.sent_at = t;
                self.sent_us = apic::micros();
            }
        } else if self.sent >= count && (self.received >= self.sent || t >= self.sent_at + timeout) {
            let lost = self.sent - self.received.min(self.sent);
            if lost > 0 {
                console::print_colored(console::YELLOW, format_args!(
                    "[WARN] ping {}: {} sent, {} replies, {} lost\n", target, self.sent, self.received, lost));
            }
            if let Some(j) = PING.lock().as_mut() {
                j.done = true;
            }
            self.active = false;
        }
    }
}

/// Runs `count` pings to `target` on the network thread and waits for them.
pub fn ping(target: Ipv4Addr, count: u16) -> Result<(), &'static str> {
    if NICS.lock().is_empty() {
        return Err("no network card");
    }
    if LEASE.lock().is_none() {
        return Err("no address yet (DHCP has not answered)");
    }
    {
        let mut job = PING.lock();
        if job.as_ref().is_some_and(|j| !j.done) {
            return Err("a ping is already running");
        }
        *job = Some(PingJob { target, count, done: false });
    }
    WAKE.signal();
    while PING.lock().as_ref().is_some_and(|j| !j.done) {
        sched::sleep_ticks(5);
    }
    Ok(())
}
