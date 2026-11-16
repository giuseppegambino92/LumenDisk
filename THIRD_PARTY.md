# Third-party components

LumenDisk is licensed under the GNU GPL v3.0 or later (see [LICENSE](LICENSE)). It includes or
embeds the following components, each under its own license.

| Component | Where | License | Source |
|---|---|---|---|
| nfsserve (modified) | `vendor/nfsserve` | BSD-3-Clause | https://github.com/xetdata/nfsserve |
| ms-sys boot records (Windows 7 MBR, NT6 FAT32 / NTFS boot code), from Rufus v4.15 (commit 6d8fbf98305ff37eb531c45cbd6ff44563c53917, `src/ms-sys/inc/`) | `core/src/forge/windows/ms_sys.rs` | GPL-2.0-or-later | https://github.com/pbatard/rufus/tree/v4.15/src/ms-sys |
| UEFI:NTFS 2.8 boot image `uefi-ntfs.img` (sha256 72683fa1250eeea772d3399277b434d4e55ba8dd0dc926e52d817e701fc2eb9e), from Rufus v4.15 `res/uefi/` | `core/src/forge/windows/res/uefi-ntfs.img` | GPL-2.0-or-later | https://github.com/pbatard/uefi-ntfs (tag v2.8, commit 42de94dce5a0be68aab4e410494bb9d986dbcac4) |
| ntfs-3g UEFI driver 1.9, inside `uefi-ntfs.img` | (embedded) | GPL-2.0-or-later | https://github.com/pbatard/ntfs-3g (commit a2226ecd8344a427a7ad79fbb878f3244c7af393) |
| SYSLINUX 6.03 (`ldlinux.sys`, `ldlinux.bss`, `mbr.bin`, BIOS `.c32` modules, unmodified release binaries) | `core/src/forge/linux/syslinux-6.03/` | GPL-2.0-or-later | https://www.kernel.org/pub/linux/utils/boot/syslinux/syslinux-6.03.tar.xz (sha256 26d3986d2bea109d5dc0e4f8c4822a459276cf021125e8c9f23c3cca5d8c850e) |
| SYSLINUX libinstaller logic (syslxmod, boot sector), ported to Rust | `core/src/forge/linux/syslinux.rs` | GPL-2.0-or-later | same tarball |
| EfiFs 1.12 exFAT and non-x86 drivers, inside `uefi-ntfs.img` | (embedded) | GPL-3.0-or-later | https://github.com/pbatard/efifs (tag v1.12, commit bfc6ae29ac15516bddb97928eaf1eda187a6d242) |

Rust crates linked into the core are listed with their licenses by `cargo license` (all are
GPL-3.0-compatible: MIT, Apache-2.0, BSD, ISC, Zlib, Unicode).
