#!/usr/bin/env python3
"""Runs QEMU monitor commands (for example device_add) and prints the replies.

Usage: qemu-monitor.py [--gap SECONDS] <monitor socket> <command> [<command> ...]

--gap sets the wait after each command (0.5 s by default); a short gap
lets mouse_button commands make a double-click.
"""
import socket
import sys
import time

args = sys.argv[1:]
gap = 0.5
if args[:1] == ['--gap']:
    gap = float(args[1])
    args = args[2:]
s = socket.socket(socket.AF_UNIX)
s.connect(args[0])
time.sleep(0.3)
s.recv(65536)
for cmd in args[1:]:
    s.sendall((cmd + '\n').encode())
    time.sleep(gap)
    print(s.recv(65536).decode(errors='replace').replace('(qemu) ', '').strip())
s.close()
