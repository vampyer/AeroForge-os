#!/usr/bin/env python3
"""Types text into a QEMU guest through the monitor's sendkey command.

Usage: qemu-type.py <monitor socket> <text>   (a trailing newline presses Enter)
"""
import socket
import sys
import time

KEYS = {' ': 'spc', '\n': 'ret', '-': 'minus', '.': 'dot', '/': 'slash', ':': 'shift-semicolon', '"': 'shift-apostrophe', '=': 'equal', '+': 'shift-equal', '*': 'shift-8'}

s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
time.sleep(0.3)
s.recv(65536)
for ch in sys.argv[2]:
    key = KEYS.get(ch, 'shift-' + ch.lower() if ch.isupper() else ch)
    s.sendall(('sendkey ' + key + '\n').encode())
    time.sleep(0.1)
s.close()
