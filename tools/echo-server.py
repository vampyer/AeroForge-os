#!/usr/bin/env python3
"""Echo server for the boot test's nettest program. In QEMU's user-mode
network, the guest reaches this host's loopback as 10.0.2.2.

UDP 5555 sends every datagram back. TCP 5556 sends back what a connection
sends and closes it after 65536 bytes, so the guest also sees the end of
the data. Nothing listens on TCP 5557: connecting there must be refused.
UDP 5553 is a DNS server that knows one name, aeroforge.test (10.0.2.2),
and answers every other A query with "no such name".

`echo-server.py client PORT` is the other direction: it connects to a
program listening in the guest (QEMU forwards host PORT to it), sends a
line and prints what comes back."""
import socket
import struct
import sys
import threading
import time

DNS_NAMES = {"aeroforge.test": bytes([10, 0, 2, 2])}

TCP_TOTAL = 65536


def udp_echo(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    while True:
        data, peer = s.recvfrom(65536)
        print(f"udp {len(data)} bytes from {peer}", flush=True)
        s.sendto(data, peer)


def dns_reply(q):
    """Answers an A query from DNS_NAMES, NXDOMAIN for anything else."""
    ident, flags, qd = struct.unpack(">HHH", q[:6])
    i, labels = 12, []
    while q[i]:
        labels.append(q[i + 1:i + 1 + q[i]].decode(errors="replace"))
        i += 1 + q[i]
    question = q[12:i + 5]
    qtype = struct.unpack(">H", q[i + 1:i + 3])[0]
    name = ".".join(labels).lower()
    addr = DNS_NAMES.get(name)
    rd = flags & 0x0100
    if addr is None:
        return name, struct.pack(">HHHHHH", ident, 0x8083 | rd, 1, 0, 0, 0) + question
    answers = 1 if qtype == 1 else 0
    reply = struct.pack(">HHHHHH", ident, 0x8080 | rd, 1, answers, 0, 0) + question
    if answers:
        # A CNAME first, as real resolvers often send, then the A record.
        reply = struct.pack(">HHHHHH", ident, 0x8080 | rd, 1, 2, 0, 0) + question
        reply += struct.pack(">HHHIH", 0xC00C, 5, 1, 60, 2) + b"\xc0\x0c"
        reply += struct.pack(">HHHIH", 0xC00C, 1, 1, 60, 4) + addr
    return name, reply


def dns_server(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    while True:
        q, peer = s.recvfrom(512)
        try:
            name, reply = dns_reply(q)
        except (IndexError, struct.error):
            continue
        print(f"dns {name} from {peer}: {'found' if name in DNS_NAMES else 'no such name'}", flush=True)
        s.sendto(reply, peer)


def client(port):
    """Connects to the guest's server until one exchange works (QEMU accepts
    the host side at once and closes it while nothing listens in the guest)."""
    line = b"hello from the host\n"
    want = b"AeroForge heard: " + line
    for _ in range(60):
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=5) as c:
                c.sendall(line)
                got = b""
                while True:
                    data = c.recv(4096)
                    if not data:
                        break
                    got += data
            if got == want:
                print(f"host client: got {got!r}: OK", flush=True)
                return
            if got:
                print(f"host client: got {got!r}, wanted {want!r}", flush=True)
        except OSError as e:
            print(f"host client: {e}", flush=True)
        time.sleep(0.5)
    print("host client: gave up", flush=True)


def tcp_conn(c, peer):
    total = 0
    with c:
        while total < TCP_TOTAL:
            data = c.recv(min(16384, TCP_TOTAL - total))
            if not data:
                break
            c.sendall(data)
            total += len(data)
    print(f"tcp {total} bytes from {peer}, closed", flush=True)


def tcp_echo(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    s.listen()
    while True:
        c, peer = s.accept()
        threading.Thread(target=tcp_conn, args=(c, peer), daemon=True).start()


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "client":
        client(int(sys.argv[2]))
        sys.exit(0)
    udp_port = int(sys.argv[1]) if len(sys.argv) > 1 else 5555
    tcp_port = int(sys.argv[2]) if len(sys.argv) > 2 else 5556
    threading.Thread(target=udp_echo, args=(udp_port,), daemon=True).start()
    threading.Thread(target=dns_server, args=(5553,), daemon=True).start()
    print(f"echo server: UDP {udp_port}, TCP {tcp_port}, DNS 5553", flush=True)
    tcp_echo(tcp_port)
