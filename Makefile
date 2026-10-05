# AeroForge OS build orchestration.
#   make            build the UEFI boot ISO (build/aeroforge.iso)
#   make run        boot it in QEMU with a window
#   make run-headless   boot with serial on stdout, no display (CI)
#   make disk       rebuild the NVMe and SATA disk images (build/disk.img, build/sata.img)
#   make clean

LIMINE_BRANCH ?= v9.x-binary
OVMF_CODE     ?= /usr/share/OVMF/OVMF_CODE_4M.fd
OVMF_VARS     ?= /usr/share/OVMF/OVMF_VARS_4M.fd
QEMU          ?= qemu-system-x86_64
QEMU_FLAGS    ?= -M q35 -cpu max -m 512M -smp 4 -no-reboot -no-shutdown

BUILD  := build
KERNEL := kernel/target/x86_64-unknown-none/release/aerokernel
USERBIN := userland/target/x86_64-unknown-none/release
PROGRAMS := aerosmss echod client crasher sectest nxtest rotest
ISO    := $(BUILD)/aeroforge.iso
LIMINE := $(BUILD)/limine

.PHONY: all kernel userland iso disk run run-headless clean
all: iso

kernel:
	cd kernel && cargo build --release

userland:
	cd userland && cargo build --release

$(LIMINE)/BOOTX64.EFI:
	rm -rf $(LIMINE)
	git clone --depth 1 --branch $(LIMINE_BRANCH) https://github.com/limine-bootloader/limine.git $(LIMINE)

iso: kernel userland $(LIMINE)/BOOTX64.EFI
	rm -rf $(BUILD)/iso_root
	mkdir -p $(BUILD)/iso_root/boot/limine $(BUILD)/iso_root/boot/bin $(BUILD)/iso_root/EFI/BOOT
	cp $(KERNEL) $(BUILD)/iso_root/boot/aerokernel
	for p in $(PROGRAMS); do cp $(USERBIN)/$$p $(BUILD)/iso_root/boot/bin/; done
	cp boot/limine.conf $(LIMINE)/limine-uefi-cd.bin $(BUILD)/iso_root/boot/limine/
	cp $(LIMINE)/BOOTX64.EFI $(BUILD)/iso_root/EFI/BOOT/
	xorriso -as mkisofs -R -r -J \
		--efi-boot boot/limine/limine-uefi-cd.bin \
		-efi-boot-part --efi-boot-image --protective-msdos-label \
		$(BUILD)/iso_root -o $(ISO) 2>/dev/null
	@echo "Built $(ISO)"

disk:
	./tools/make-disk.sh
	./tools/make-sata-disk.sh

$(BUILD)/disk.img:
	./tools/make-disk.sh

$(BUILD)/sata.img:
	./tools/make-sata-disk.sh

$(BUILD)/vars.fd:
	mkdir -p $(BUILD)
	cp $(OVMF_VARS) $@

OVMF_ARGS = -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
            -drive if=pflash,format=raw,file=$(BUILD)/vars.fd
NVME_ARGS = -drive file=$(BUILD)/disk.img,if=none,id=nvm,format=raw \
            -device nvme,serial=AERO0001,drive=nvm \
            -drive file=$(BUILD)/sata.img,if=none,id=sata,format=raw \
            -device ide-hd,drive=sata,bus=ide.1,serial=AEROSATA1 \
            -device qemu-xhci,id=xhci -device usb-hub,bus=xhci.0,port=1 \
            -device usb-kbd,bus=xhci.0,port=1.1 -device usb-mouse,bus=xhci.0,port=1.2

run: iso $(BUILD)/vars.fd $(BUILD)/disk.img $(BUILD)/sata.img
	$(QEMU) $(QEMU_FLAGS) $(OVMF_ARGS) $(NVME_ARGS) -cdrom $(ISO) -serial stdio

run-headless: iso $(BUILD)/vars.fd $(BUILD)/disk.img $(BUILD)/sata.img
	$(QEMU) $(QEMU_FLAGS) $(OVMF_ARGS) $(NVME_ARGS) -cdrom $(ISO) -serial stdio -display none

clean:
	rm -rf $(BUILD)
	cd kernel && cargo clean
	cd userland && cargo clean
