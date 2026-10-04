This file lives on a FAT32 partition of an NVMe disk image.
AeroKernel found the NVMe controller on PCIe, brought it up through the
C++ NVMe driver (behind the Driver Host Interface), read the GPT partition
table, mounted the FAT32 volume and read this text from it.
