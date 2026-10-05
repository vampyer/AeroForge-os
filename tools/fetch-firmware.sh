#!/usr/bin/env bash
# Downloads the MediaTek Bluetooth firmware (MT7921 and MT7922 adapters boot
# with a ROM only and need this patch) from linux-firmware, pinned to one
# commit and checked against known SHA-256 sums. The files are redistributable
# for use with MediaTek devices (build/firmware/LICENCE.mediatek); they are not
# kept in this repository.
set -euo pipefail
cd "$(dirname "$0")/.."

COMMIT=d947e4e8e314e9254a1242dc1a5d9cede2cce33d
BASE=${LINUX_FIRMWARE_URL:-https://gitlab.com/kernel-firmware/linux-firmware/-/raw/$COMMIT}
OUT=build/firmware

FILES=(
    "mediatek/BT_RAM_CODE_MT7961_1_2_hdr.bin 7a440a0f7404c73a2f03d8cc0475ac9f7fdc931b494dce37705e7d578153dbae"
    "mediatek/BT_RAM_CODE_MT7961_1a_2_hdr.bin 671d18fd594a73bc4747c1922b17271878c8bb26fa8fd122238e797d52e337e9"
    "mediatek/BT_RAM_CODE_MT7922_1_1_hdr.bin 299942dbec7d34af030670ea59f9f7b4fd5c64e738b9e0157d76f4dd3beb190f"
    "LICENSES/LICENCE.mediatek a90d3f66704d85889945fec5525ea77622549da83aced1aac99828383f8f1805"
)

for entry in "${FILES[@]}"; do
    read -r path sum <<<"$entry"
    dest="$OUT/${path#LICENSES/}"
    if [ -f "$dest" ] && echo "$sum  $dest" | sha256sum -c --quiet 2>/dev/null; then
        continue
    fi
    mkdir -p "$(dirname "$dest")"
    curl -fsSL --retry 3 -o "$dest.part" "$BASE/$path"
    if ! echo "$sum  $dest.part" | sha256sum -c --quiet; then
        echo "fetch-firmware: $path does not match its pinned SHA-256" >&2
        rm -f "$dest.part"
        exit 1
    fi
    mv "$dest.part" "$dest"
    echo "fetched $path"
done
