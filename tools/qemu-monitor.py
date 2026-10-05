#!/usr/bin/env python3
"""Runs QEMU monitor commands (for example device_add) and prints the replies.

Usage: qemu-monitor.py <monitor socket> <command> [<command> ...]
"""
import socket
import sys
import time

s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
time.sleep(0.3)
s.recv(65536)
for cmd in sys.argv[2:]:
    s.sendall((cmd + '\n').encode())
    time.sleep(0.5)
    print(s.recv(65536).decode(errors='replace').replace('(qemu) ', '').strip())
s.close()
