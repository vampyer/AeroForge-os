//! Networking: finds Intel Ethernet controllers, brings them up through the
//! C++ e1000 driver and runs a smoltcp TCP/IP stack on the first one from a
//! kernel thread. DHCP configures the address; `ping` sends ICMP echoes.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU64, Ordering};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::socket::{dhcpv4, icmp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, Icmpv4Packet, Icmpv4Repr, IpAddress, IpCidr, Ipv4Cidr};

use crate::dhi::{self, NetInfo};
use crate::sync::IrqMutex;
use crate::{apic, console, pci, sched};

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
        nics.push(Nic { port: Port { driver, id }, driver_name, name, location: at, pci_id: (dev.vendor, dev.device), info });
        found += 1;
    }
    found
}

pub fn mac_string(mac: &[u8; 6]) -> String {
    alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

fn now() -> Instant {
    Instant::from_millis((sched::ticks() * 1000 / apic::TIMER_HZ) as i64)
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
    let Some((nic, mac)) = NICS.lock().first().map(|n| (n.port, n.info.mac)) else { return };
    let mut device = Device { nic, rx: [0; 2048] };
    let config = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
    let mut iface = Interface::new(config, &mut device, now());

    let mut sockets = SocketSet::new(vec![]);
    let dhcp = sockets.add(dhcpv4::Socket::new());
    let icmp_socket = icmp::Socket::new(
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
        icmp::PacketBuffer::new(vec![icmp::PacketMetadata::EMPTY; 8], vec![0; 2048]),
    );
    let icmp = sockets.add(icmp_socket);
    sockets.get_mut::<icmp::Socket>(icmp).bind(icmp::Endpoint::Ident(PING_IDENT)).ok();

    let mut pinger = Pinger::default();
    let mut pinged_gateway = false;
    loop {
        iface.poll(now(), &mut device, &mut sockets);

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
                    if let (Some(r), false) = (cfg.router, pinged_gateway) {
                        pinged_gateway = true;
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

        pinger.step(&mut sockets, icmp, &ChecksumCapabilities::default());
        sched::sleep_ticks(1);
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
                let ms = (t - self.sent_at) * 1000 / apic::TIMER_HZ;
                crate::kok!("ping {}: reply from {}, seq {}, time {} ms", target, from, seq_no, ms);
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
    while PING.lock().as_ref().is_some_and(|j| !j.done) {
        sched::sleep_ticks(5);
    }
    Ok(())
}
