#!/usr/bin/env python3
"""Sends input to a QEMU guest over QMP, for what the human monitor cannot do.

Usage: qemu-qmp.py <qmp socket> abs <x> <y>   (absolute pointer, 0..32767 each)
       qemu-qmp.py <qmp socket> down <key>     (hold a key down: ctrl, shift, alt, ...)
       qemu-qmp.py <qmp socket> up <key>       (let it go)
"""
import json
import socket
import sys

s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])
f = s.makefile('rw')


def call(cmd, **args):
    f.write(json.dumps({'execute': cmd, 'arguments': args} if args else {'execute': cmd}) + '\n')
    f.flush()
    while True:
        reply = json.loads(f.readline())
        if 'return' in reply or 'error' in reply:
            if 'error' in reply:
                sys.exit(f"qmp {cmd}: {reply['error']}")
            return reply['return']


json.loads(f.readline())  # greeting
call('qmp_capabilities')
if sys.argv[2] == 'abs':
    x, y = int(sys.argv[3]), int(sys.argv[4])
    call('input-send-event', events=[{'type': 'abs', 'data': {'axis': 'x', 'value': x}},
                                     {'type': 'abs', 'data': {'axis': 'y', 'value': y}}])
elif sys.argv[2] in ('down', 'up'):
    call('input-send-event', events=[{'type': 'key', 'data': {'down': sys.argv[2] == 'down',
                                                              'key': {'type': 'qcode', 'data': sys.argv[3]}}}])
s.close()
