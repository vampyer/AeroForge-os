#!/bin/bash
# Starts a Samba server for the boot test's network drive checks: share
# "aero" (build/smbshare) on 127.0.0.1 port 4450 (10.0.2.2:4450 inside
# QEMU), user aerotest, password Forge-pass-7, SMB 2 or newer, signing
# required (as Windows 11 does). Needs root (or sudo) for smbd and the
# user account. Prints smbd's process id.
set -euo pipefail
cd "$(dirname "$0")/.."
SUDO=$([ "$(id -u)" = 0 ] && echo "" || echo sudo)
D=$PWD/build/samba
# One left from an earlier run holds the port.
[ -s "$D/pid/smbd.pid" ] && $SUDO kill "$(cat "$D/pid/smbd.pid")" 2>/dev/null && sleep 0.5 || true
$SUDO rm -rf "$D" build/smbshare
mkdir -p "$D"/{private,lock,state,cache,pid} build/smbshare/Music
printf 'Hello from the Samba share.\n' > build/smbshare/hello.txt
printf 'track one' > build/smbshare/Music/track1.txt
id aerotest >/dev/null 2>&1 || $SUDO useradd -M -s /usr/sbin/nologin aerotest
# Files on the share are read and written as whoever ran this script (it
# owns build/smbshare, and aerotest may not even get into its home folder).
OWNER=$(id -un)
cat > "$D/smb.conf" <<CONF
[global]
    server role = standalone server
    smb ports = 4450
    interfaces = lo
    bind interfaces only = yes
    disable netbios = yes
    server min protocol = SMB2_02
    server signing = mandatory
    map to guest = never
    passdb backend = tdbsam:$D/private/passdb.tdb
    private dir = $D/private
    lock directory = $D/lock
    state directory = $D/state
    cache directory = $D/cache
    pid directory = $D/pid
    ncalrpc dir = $D/ncalrpc
    log file = $D/smbd.log
    log level = 1
    load printers = no
    printing = bsd
    printcap name = /dev/null
[aero]
    path = $PWD/build/smbshare
    read only = no
    valid users = aerotest
    force user = $OWNER
CONF
printf 'Forge-pass-7\nForge-pass-7\n' | $SUDO smbpasswd -c "$D/smb.conf" -s -a aerotest >/dev/null 2>&1
$SUDO smbd -s "$D/smb.conf" -D
for _ in $(seq 50); do [ -s "$D/pid/smbd.pid" ] && break; sleep 0.1; done
cat "$D/pid/smbd.pid"
