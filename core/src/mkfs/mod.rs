//! `mkfs` for filesystems LumenDisk creates but does not mount: Btrfs, XFS, F2FS, UFS2.
//! Each `format(dev, opts)` writes a fresh, empty filesystem accepted by the upstream checker
//! (`btrfs check`, `xfs_repair -n`, `fsck.f2fs`, FreeBSD `fsck_ffs`) and mountable by Linux /
//! FreeBSD.

pub mod btrfs;
pub mod f2fs;
pub mod ufs;
pub mod xfs;
