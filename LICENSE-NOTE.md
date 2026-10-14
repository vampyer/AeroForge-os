AeroForge's own code is offered under MIT OR Apache-2.0, as the design doc recommends.
Third-party pieces: Limine (BSD-2-Clause, downloaded at build time, not vendored),
font8x8 (MIT, public-domain glyph data), spin and linked_list_allocator (MIT/Apache-2.0).
The picture and sound decoders: zune-jpeg, zune-png, zune-core and zune-inflate (MIT/Apache-2.0/Zlib),
simd-adler32 (MIT), nanomp3 and nanomp3-core (MIT/Apache-2.0), lewton (MIT/Apache-2.0; kept in
userland/vendor/lewton with its LICENSE, changed to build without std) and tinyvec (Zlib/Apache-2.0/MIT).
The FLAC decoder (userland/src/flac.rs) is AeroForge's own. The sample pictures and songs on the
test disk are made by tools/make-media.py.

MediaTek Bluetooth firmware (BT_RAM_CODE_MT7961/MT7922) comes from linux-firmware and is
downloaded at build time, not kept in this repository. MediaTek allows its redistribution for
use with devices containing MediaTek chipsets (LICENCE.mediatek, copied onto the ISO next to
the files). It is not covered by AeroForge's licence. drivers/btmtk reimplements the download procedure of
Linux's btmtk.c (GPL-2.0) in C++, and its USB id list comes from Linux's btusb.c; whether that
makes it a derivative of GPL code needs checking before a release under MIT/Apache-2.0.
