#!/usr/bin/env python3
"""Echo server for the boot test's nettest program. In QEMU's user-mode
network, the guest reaches this host's loopback as 10.0.2.2.

UDP 5555 sends every datagram back. TCP 5556 sends back what a connection
sends and closes it after 65536 bytes, so the guest also sees the end of
the data. Nothing listens on TCP 5557: connecting there must be refused."""
import socket
import sys
import threading

TCP_TOTAL = 65536


def udp_echo(port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port))
    while True:
        data, peer = s.recvfrom(65536)
        print(f"udp {len(data)} bytes from {peer}", flush=True)
        s.sendto(data, peer)


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
    udp_port = int(sys.argv[1]) if len(sys.argv) > 1 else 5555
    tcp_port = int(sys.argv[2]) if len(sys.argv) > 2 else 5556
    threading.Thread(target=udp_echo, args=(udp_port,), daemon=True).start()
    print(f"echo server: UDP {udp_port}, TCP {tcp_port}", flush=True)
    tcp_echo(tcp_port)
