//! Block & image abstraction layer.
//!
//! * [`FileDev`]  – a disk image file or a raw device node opened through a file descriptor
//!   (possibly handed to us by `/usr/libexec/authopen`, see [`authopen_fd`]).
//! * [`AlignedDev`] – read-modify-write shim for raw `/dev/rdiskN` nodes, which only accept
//!   sector-aligned I/O.
//! * [`PartitionDev`] – a window (offset/length) onto another device.
//! * [`MemDev`] – RAM-backed device for unit tests.
//! * [`parse_partitions`] – GPT and MBR (incl. extended/logical) partition table parser.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub trait BlockDev: Send + Sync {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()>;
    fn size(&self) -> u64;
    fn flush(&self) -> io::Result<()>;
    fn read_only(&self) -> bool;

    /// Write **file payload** (as opposed to filesystem metadata). Journaling layers write it
    /// in place immediately and keep it out of the redo log (ordered mode, like ext4
    /// `data=ordered`); everything else treats it as a normal write. Drivers must use it only
    /// for the contents of regular files (and zero-fill of newly allocated data blocks).
    fn write_data_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.write_at(offset, buf)
    }

    /// The filesystem just marked this byte range free (blocks / clusters). Journaling layers
    /// must not let payload overwrite it in place until the freeing transaction is committed,
    /// because the last committed metadata may still reference it. Default: nothing to do.
    fn note_freed(&self, _offset: u64, _len: u64) {}

    /// Convenience helper.
    fn read_vec(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read_at(offset, &mut v)?;
        Ok(v)
    }
}

fn ro_err() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "device opened read-only")
}

fn bounds(offset: u64, len: usize, size: u64) -> io::Result<()> {
    if offset.checked_add(len as u64).map_or(true, |e| e > size) {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!("I/O beyond end of device (off={offset} len={len} size={size})"),
        ));
    }
    Ok(())
}

/// Image file or device node.
pub struct FileDev {
    file: File,
    size: u64,
    ro: bool,
}

impl FileDev {
    pub fn open(path: &Path, read_only: bool) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(!read_only).open(path)?;
        Self::from_file(file, read_only)
    }

    /// Wrap a descriptor obtained from authopen / DiskArbitration. Takes ownership of `fd`.
    pub fn from_fd(fd: RawFd, read_only: bool) -> io::Result<Self> {
        // SAFETY: the caller transfers ownership of a valid open descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        Self::from_file(file, read_only)
    }

    fn from_file(file: File, ro: bool) -> io::Result<Self> {
        let size = device_size(&file)?;
        Ok(FileDev { file, size, ro })
    }
}

/// Size of a regular file or of a block/char device (via DKIOCGETBLOCKCOUNT).
fn device_size(file: &File) -> io::Result<u64> {
    let md = file.metadata()?;
    if md.is_file() {
        return Ok(md.len());
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // <sys/disk.h>: DKIOCGETBLOCKSIZE _IOR('d', 24, uint32_t), DKIOCGETBLOCKCOUNT _IOR('d', 25, uint64_t)
        const DKIOCGETBLOCKSIZE: libc::c_ulong = 0x40046418;
        const DKIOCGETBLOCKCOUNT: libc::c_ulong = 0x40086419;
        let mut bs: u32 = 0;
        let mut bc: u64 = 0;
        // SAFETY: ioctls write into the provided properly sized out-params.
        unsafe {
            if libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKSIZE, &mut bs) == 0
                && libc::ioctl(file.as_raw_fd(), DKIOCGETBLOCKCOUNT, &mut bc) == 0
            {
                return Ok(bs as u64 * bc);
            }
        }
    }
    Err(io::Error::new(io::ErrorKind::Other, "cannot determine device size"))
}

impl BlockDev for FileDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        bounds(offset, buf.len(), self.size)?;
        self.file.read_exact_at(buf, offset)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        if self.ro {
            return Err(ro_err());
        }
        bounds(offset, buf.len(), self.size)?;
        self.file.write_all_at(buf, offset)
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn flush(&self) -> io::Result<()> {
        if self.ro {
            return Ok(());
        }
        match self.file.sync_data() {
            Ok(()) => Ok(()),
            // Raw disk nodes don't support F_FULLFSYNC (ENOTTY): fsync + ask the drive to
            // flush its write cache (DKIOCSYNCHRONIZECACHE).
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOTTY) | Some(libc::ENOTSUP) | Some(libc::EINVAL)) => {
                use std::os::fd::AsRawFd;
                const DKIOCSYNCHRONIZECACHE: libc::c_ulong = 0x2000_6416; // _IO('d', 22)
                // SAFETY: plain syscalls on a valid descriptor.
                unsafe {
                    if libc::fsync(self.file.as_raw_fd()) != 0 {
                        let err = io::Error::last_os_error();
                        if !matches!(err.raw_os_error(), Some(libc::ENOTTY) | Some(libc::ENOTSUP)) {
                            return Err(err);
                        }
                    }
                    libc::ioctl(self.file.as_raw_fd(), DKIOCSYNCHRONIZECACHE);
                }
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
    fn read_only(&self) -> bool {
        self.ro
    }
}

/// Makes arbitrary byte-granular I/O work on raw devices that require sector alignment.
pub struct AlignedDev {
    inner: Arc<dyn BlockDev>,
    sector: u64,
    lock: Mutex<()>,
}

impl AlignedDev {
    pub fn new(inner: Arc<dyn BlockDev>, sector: u64) -> Self {
        AlignedDev { inner, sector: sector.max(512), lock: Mutex::new(()) }
    }
    fn span(&self, offset: u64, len: usize) -> (u64, u64) {
        let start = offset / self.sector * self.sector;
        let end = (offset + len as u64).div_ceil(self.sector) * self.sector;
        (start, end)
    }
}

impl BlockDev for AlignedDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let (s, e) = self.span(offset, buf.len());
        if s == offset && e == offset + buf.len() as u64 {
            return self.inner.read_at(offset, buf);
        }
        let mut tmp = vec![0u8; (e - s) as usize];
        self.inner.read_at(s, &mut tmp)?;
        let o = (offset - s) as usize;
        buf.copy_from_slice(&tmp[o..o + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let (s, e) = self.span(offset, buf.len());
        if s == offset && e == offset + buf.len() as u64 {
            return self.inner.write_at(offset, buf);
        }
        let _g = self.lock.lock().unwrap();
        let mut tmp = vec![0u8; (e - s) as usize];
        self.inner.read_at(s, &mut tmp)?;
        let o = (offset - s) as usize;
        tmp[o..o + buf.len()].copy_from_slice(buf);
        self.inner.write_at(s, &tmp)
    }
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }
    fn note_freed(&self, offset: u64, len: u64) {
        self.inner.note_freed(offset, len)
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
}

/// A window onto a parent device.
pub struct PartitionDev {
    inner: Arc<dyn BlockDev>,
    offset: u64,
    len: u64,
}

impl PartitionDev {
    pub fn new(inner: Arc<dyn BlockDev>, offset: u64, len: u64) -> io::Result<Self> {
        if offset + len > inner.size() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "partition exceeds device"));
        }
        Ok(PartitionDev { inner, offset, len })
    }
}

impl BlockDev for PartitionDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        bounds(offset, buf.len(), self.len)?;
        self.inner.read_at(self.offset + offset, buf)
    }
    fn write_data_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        bounds(offset, buf.len(), self.len)?;
        self.inner.write_data_at(self.offset + offset, buf)
    }
    fn note_freed(&self, offset: u64, len: u64) {
        self.inner.note_freed(self.offset + offset, len)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        bounds(offset, buf.len(), self.len)?;
        self.inner.write_at(self.offset + offset, buf)
    }
    fn size(&self) -> u64 {
        self.len
    }
    fn flush(&self) -> io::Result<()> {
        self.inner.flush()
    }
    fn read_only(&self) -> bool {
        self.inner.read_only()
    }
}

/// RAM-backed device (tests, scratch formatting).
pub struct MemDev {
    data: Mutex<Vec<u8>>,
}

impl MemDev {
    pub fn new(size: usize) -> Self {
        MemDev { data: Mutex::new(vec![0u8; size]) }
    }
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }
}

impl BlockDev for MemDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let d = self.data.lock().unwrap();
        bounds(offset, buf.len(), d.len() as u64)?;
        buf.copy_from_slice(&d[offset as usize..offset as usize + buf.len()]);
        Ok(())
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        let mut d = self.data.lock().unwrap();
        bounds(offset, buf.len(), d.len() as u64)?;
        d[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        false
    }
}

// ───────────────────────────── Partition tables ─────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct PartitionInfo {
    /// 1-based index as shown to the user (disk4s<index>).
    pub index: u32,
    pub start: u64,
    pub size: u64,
    pub scheme: String,
    /// Human readable type ("Linux filesystem", "Microsoft basic data", "FAT32 (LBA)", ...).
    pub type_name: String,
    /// Raw type (MBR byte as hex or GPT GUID).
    pub type_id: String,
    /// GPT partition name (UTF-16) or empty.
    pub name: String,
}

const SECTOR: u64 = 512;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

fn guid_string(b: &[u8]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        le32(b, 0),
        le16(b, 4),
        le16(b, 6),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

fn gpt_type_name(guid: &str) -> &'static str {
    match guid {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI System",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux filesystem",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "B921B045-1DF0-41C3-AF44-4C6F280D3FAE" => "Linux root (ARM64)",
        "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709" => "Linux root (x86-64)",
        _ => "Unknown",
    }
}

fn mbr_type_name(t: u8) -> &'static str {
    match t {
        0x01 => "FAT12",
        0x04 | 0x06 | 0x0E => "FAT16",
        0x07 => "NTFS / exFAT",
        0x0B | 0x0C => "FAT32",
        0x05 | 0x0F | 0x85 => "Extended",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x8E => "Linux LVM",
        0xA8 => "Apple UFS",
        0xAF => "Apple HFS",
        0xEE => "GPT protective",
        0xEF => "EFI System",
        _ => "Unknown",
    }
}

/// Parse GPT (preferred) or MBR partition tables. Returns an empty vector for a
/// "superfloppy" (filesystem directly on the device, no table).
pub fn parse_partitions(dev: &dyn BlockDev) -> io::Result<Vec<PartitionInfo>> {
    if dev.size() < 2 * SECTOR {
        return Ok(vec![]);
    }
    let mbr = dev.read_vec(0, 512)?;
    if mbr[510] != 0x55 || mbr[511] != 0xAA {
        return Ok(vec![]);
    }
    let has_protective = (0..4).any(|i| mbr[446 + i * 16 + 4] == 0xEE);
    if has_protective {
        for &ss in &[512u64, 4096] {
            if let Some(v) = parse_gpt(dev, ss)? {
                return Ok(v);
            }
        }
    }
    // A FAT/NTFS/exFAT boot sector also ends with 0x55AA; distinguish from an MBR.
    if looks_like_boot_sector(&mbr) {
        return Ok(vec![]);
    }
    parse_mbr(dev, &mbr)
}

fn looks_like_boot_sector(b: &[u8]) -> bool {
    let oem = &b[3..11];
    if oem == b"NTFS    " || oem == b"EXFAT   " {
        return true;
    }
    // FAT BPB: jump instruction + sane bytes/sector + "FAT" signature string.
    let jmp = b[0] == 0xEB || b[0] == 0xE9;
    let bps = le16(b, 11);
    let fat_sig = &b[54..57] == b"FAT" || &b[82..85] == b"FAT";
    jmp && matches!(bps, 512 | 1024 | 2048 | 4096) && fat_sig
}

fn parse_gpt(dev: &dyn BlockDev, ss: u64) -> io::Result<Option<Vec<PartitionInfo>>> {
    if dev.size() < ss * 2 {
        return Ok(None);
    }
    let hdr = dev.read_vec(ss, 92)?;
    if &hdr[0..8] != b"EFI PART" {
        return Ok(None);
    }
    let entries_lba = le64(&hdr, 72);
    let n = le32(&hdr, 80).min(1024) as u64;
    let esz = le32(&hdr, 84) as u64;
    if esz < 128 || esz > 4096 {
        return Ok(None);
    }
    let table = dev.read_vec(entries_lba * ss, (n * esz) as usize)?;
    let mut out = vec![];
    for i in 0..n {
        let e = &table[(i * esz) as usize..((i + 1) * esz) as usize];
        if e[0..16].iter().all(|&b| b == 0) {
            continue;
        }
        let type_guid = guid_string(&e[0..16]);
        let first = le64(e, 32);
        let last = le64(e, 40);
        if last < first {
            continue;
        }
        let name_u16: Vec<u16> = (0..36).map(|k| le16(e, 56 + k * 2)).take_while(|&c| c != 0).collect();
        out.push(PartitionInfo {
            index: i as u32 + 1,
            start: first * ss,
            size: (last - first + 1) * ss,
            scheme: "GPT".into(),
            type_name: gpt_type_name(&type_guid).into(),
            type_id: type_guid,
            name: String::from_utf16_lossy(&name_u16),
        });
    }
    Ok(Some(out))
}

fn parse_mbr(dev: &dyn BlockDev, mbr: &[u8]) -> io::Result<Vec<PartitionInfo>> {
    let mut out = vec![];
    let mut ext_base = None;
    for i in 0..4 {
        let e = &mbr[446 + i * 16..446 + i * 16 + 16];
        let t = e[4];
        let lba = le32(e, 8) as u64;
        let cnt = le32(e, 12) as u64;
        if t == 0 || cnt == 0 {
            continue;
        }
        if matches!(t, 0x05 | 0x0F | 0x85) {
            ext_base = Some(lba);
            continue;
        }
        out.push(PartitionInfo {
            index: i as u32 + 1,
            start: lba * SECTOR,
            size: cnt * SECTOR,
            scheme: "MBR".into(),
            type_name: mbr_type_name(t).into(),
            type_id: format!("0x{t:02X}"),
            name: String::new(),
        });
    }
    // Logical partitions (EBR chain).
    if let Some(base) = ext_base {
        let mut ebr_lba = base;
        let mut idx = 5;
        for _ in 0..128 {
            if (ebr_lba + 1) * SECTOR > dev.size() {
                break;
            }
            let ebr = dev.read_vec(ebr_lba * SECTOR, 512)?;
            if ebr[510] != 0x55 || ebr[511] != 0xAA {
                break;
            }
            let e = &ebr[446..462];
            let (t, lba, cnt) = (e[4], le32(e, 8) as u64, le32(e, 12) as u64);
            if t != 0 && cnt != 0 {
                out.push(PartitionInfo {
                    index: idx,
                    start: (ebr_lba + lba) * SECTOR,
                    size: cnt * SECTOR,
                    scheme: "MBR".into(),
                    type_name: mbr_type_name(t).into(),
                    type_id: format!("0x{t:02X}"),
                    name: String::new(),
                });
                idx += 1;
            }
            let next = &ebr[462..478];
            let nlba = le32(next, 8) as u64;
            if next[4] == 0 || nlba == 0 {
                break;
            }
            ebr_lba = base + nlba;
        }
    }
    out.retain(|p| p.start + p.size <= dev.size());
    Ok(out)
}

// ───────────────────────────── authopen ─────────────────────────────

/// Ask macOS for an authorised descriptor on a raw device node using the system's own
/// `/usr/libexec/authopen` helper (shows the standard admin / Touch ID prompt). The helper
/// passes the open descriptor back over a UNIX socket via SCM_RIGHTS, so LumenDisk itself
/// never runs privileged code.
pub fn authopen_fd(path: &str, read_write: bool) -> io::Result<RawFd> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};

    let (ours, theirs) = UnixStream::pair()?;
    let flags = if read_write { libc::O_RDWR } else { libc::O_RDONLY };
    let theirs_fd = theirs.into_raw_fd();
    // SAFETY: theirs_fd is a valid socket we own; Stdio takes ownership.
    let child_stdout = unsafe { Stdio::from_raw_fd(theirs_fd) };
    let mut child = Command::new("/usr/libexec/authopen")
        .arg("-stdoutpipe")
        .arg("-o")
        .arg(flags.to_string())
        .arg(path)
        .stdout(child_stdout)
        .stdin(Stdio::null())
        .spawn()?;

    // Receive the descriptor.
    let mut data = [0u8; 64];
    let mut cmsg_buf = [0u8; 64];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr() as *mut _, iov_len: data.len() };
    // SAFETY: zeroed msghdr is a valid initial state; buffers outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_buf.len() as _;
    // SAFETY: recvmsg on a valid socket with valid buffers.
    let n = unsafe { libc::recvmsg(ours.as_raw_fd(), &mut msg, 0) };
    let status = child.wait()?;
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: walk the control messages returned by the kernel.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let fd = *(libc::CMSG_DATA(c) as *const RawFd);
                return Ok(fd);
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("authopen did not return a descriptor ({status})"),
    ))
}

/// Open a source path: disk image files directly, `/dev/*` nodes directly if we already have
/// access, otherwise through authopen.
pub fn open_source(path: &str, read_only: bool, passed_fd: Option<RawFd>) -> io::Result<Arc<dyn BlockDev>> {
    let is_dev = path.starts_with("/dev/");
    let base: Arc<dyn BlockDev> = if let Some(fd) = passed_fd {
        Arc::new(FileDev::from_fd(fd, read_only)?)
    } else if is_dev {
        match FileDev::open(Path::new(path), read_only) {
            Ok(d) => Arc::new(d),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let fd = authopen_fd(path, !read_only)?;
                Arc::new(FileDev::from_fd(fd, read_only)?)
            }
            Err(e) => return Err(e),
        }
    } else {
        Arc::new(FileDev::open(Path::new(path), read_only)?)
    };
    if is_dev || passed_fd.is_some() {
        Ok(Arc::new(AlignedDev::new(base, 4096)))
    } else {
        Ok(base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mbr_two_partitions() {
        let dev = MemDev::new(8 * 1024 * 1024);
        let mut mbr = vec![0u8; 512];
        // p1: FAT32 LBA at 2048, 4096 sectors
        mbr[446 + 4] = 0x0C;
        mbr[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&4096u32.to_le_bytes());
        // p2: Linux at 6144, 8192 sectors
        mbr[462 + 4] = 0x83;
        mbr[462 + 8..462 + 12].copy_from_slice(&6144u32.to_le_bytes());
        mbr[462 + 12..462 + 16].copy_from_slice(&8192u32.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        dev.write_at(0, &mbr).unwrap();
        let p = parse_partitions(&dev).unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].start, 2048 * 512);
        assert_eq!(p[1].type_name, "Linux");
    }

    #[test]
    fn aligned_rmw() {
        let mem: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 * 1024));
        let a = AlignedDev::new(mem.clone(), 4096);
        a.write_at(4095, b"xyz").unwrap();
        let mut b = [0u8; 3];
        a.read_at(4095, &mut b).unwrap();
        assert_eq!(&b, b"xyz");
    }
}

// ───────────────────────────── remote range streaming ─────────────────────────────

/// A remote byte range provider (HTTP `Range:` requests, SFTP offset reads, …).
pub trait RangeSource: Send + Sync {
    fn describe(&self) -> String;
    /// Total size of the remote object in bytes.
    fn size(&self) -> u64;
    /// Fetch exactly `len` bytes at `offset` (callers never ask past EOF).
    fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
}

/// Counters exposed to the UI and the tests ("sparse block fetch counts").
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct RangeStats {
    pub size: u64,
    pub requests: u64,
    pub fetched_bytes: u64,
    pub cache_hits: u64,
    pub cached_bytes: u64,
}

/// Read-only block device over a [`RangeSource`] with a 64 KiB chunk cache (LRU eviction,
/// bounded memory) and sequential read-ahead. Writes are refused: remote images are always
/// mounted under a Shadow overlay, so the remote file is never mutated.
pub struct CachedRangeDev {
    src: Box<dyn RangeSource>,
    size: u64,
    inner: Mutex<RangeCache>,
}

struct RangeCache {
    chunks: std::collections::HashMap<u64, (Arc<Vec<u8>>, u64)>,
    tick: u64,
    max_chunks: usize,
    last_chunk: Option<u64>,
    stats: RangeStats,
}

pub const RANGE_CHUNK: u64 = 64 * 1024;

impl CachedRangeDev {
    /// `cache_mb`: memory budget for cached chunks.
    pub fn new(src: Box<dyn RangeSource>, cache_mb: usize) -> Self {
        let size = src.size();
        CachedRangeDev {
            src,
            size,
            inner: Mutex::new(RangeCache {
                chunks: Default::default(),
                tick: 0,
                max_chunks: (cache_mb.max(4) * 1024 * 1024 / RANGE_CHUNK as usize).max(64),
                last_chunk: None,
                stats: RangeStats { size, ..Default::default() },
            }),
        }
    }

    pub fn stats(&self) -> RangeStats {
        let g = self.inner.lock().unwrap();
        let mut s = g.stats;
        s.cached_bytes = g.chunks.values().map(|(c, _)| c.len() as u64).sum();
        s
    }

    pub fn describe(&self) -> String {
        self.src.describe()
    }

    /// Get chunk `idx`, fetching `idx..idx+n` in one request on a miss (read-ahead).
    fn chunk(&self, idx: u64) -> io::Result<Arc<Vec<u8>>> {
        let ahead = {
            let mut g = self.inner.lock().unwrap();
            g.tick += 1;
            let t = g.tick;
            if let Some(e) = g.chunks.get_mut(&idx) {
                e.1 = t;
                let c = e.0.clone();
                g.stats.cache_hits += 1;
                g.last_chunk = Some(idx);
                return Ok(c);
            }
            // Sequential access → prefetch 8 chunks (512 KiB), random → 1.
            let seq = g.last_chunk.map(|l| l + 1 == idx).unwrap_or(false);
            g.last_chunk = Some(idx);
            if seq { 8 } else { 1 }
        };
        let total_chunks = self.size.div_ceil(RANGE_CHUNK);
        let mut n = ahead.min(total_chunks - idx);
        // don't refetch chunks we already have
        {
            let g = self.inner.lock().unwrap();
            for k in 1..n {
                if g.chunks.contains_key(&(idx + k)) {
                    n = k;
                    break;
                }
            }
        }
        let off = idx * RANGE_CHUNK;
        let len = ((n * RANGE_CHUNK).min(self.size - off)) as usize;
        let data = self.src.fetch(off, len)?;
        if data.len() != len {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short range response"));
        }
        let mut g = self.inner.lock().unwrap();
        g.stats.requests += 1;
        g.stats.fetched_bytes += len as u64;
        let mut first = None;
        for k in 0..n {
            let s = (k * RANGE_CHUNK) as usize;
            let e = (s + RANGE_CHUNK as usize).min(data.len());
            let c = Arc::new(data[s..e].to_vec());
            g.tick += 1;
            let t = g.tick;
            g.chunks.insert(idx + k, (c.clone(), t));
            if k == 0 {
                first = Some(c);
            }
        }
        while g.chunks.len() > g.max_chunks {
            let victim = g.chunks.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k).unwrap();
            g.chunks.remove(&victim);
        }
        Ok(first.unwrap())
    }
}

impl BlockDev for CachedRangeDev {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        bounds(offset, buf.len(), self.size)?;
        let mut pos = offset;
        let end = offset + buf.len() as u64;
        while pos < end {
            let idx = pos / RANGE_CHUNK;
            let c = self.chunk(idx)?;
            let s = (pos - idx * RANGE_CHUNK) as usize;
            let n = ((end - pos) as usize).min(c.len() - s);
            let o = (pos - offset) as usize;
            buf[o..o + n].copy_from_slice(&c[s..s + n]);
            pos += n as u64;
        }
        Ok(())
    }
    fn write_at(&self, _: u64, _: &[u8]) -> io::Result<()> {
        Err(ro_err())
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fake(Vec<u8>, Arc<AtomicU64>);
    impl RangeSource for Fake {
        fn describe(&self) -> String {
            "fake".into()
        }
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn fetch(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
            self.1.fetch_add(len as u64, Ordering::SeqCst);
            Ok(self.0[offset as usize..offset as usize + len].to_vec())
        }
    }

    #[test]
    fn cached_range_reads_and_counts() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let fetched = Arc::new(AtomicU64::new(0));
        let dev = CachedRangeDev::new(Box::new(Fake(data.clone(), fetched.clone())), 8);
        let mut b = vec![0u8; 1000];
        dev.read_at(2_500_000, &mut b).unwrap();
        assert_eq!(&b[..], &data[2_500_000..2_501_000]);
        dev.read_at(2_500_100, &mut b).unwrap();
        let s = dev.stats();
        assert_eq!(s.requests, 1);
        assert!(s.cache_hits >= 1);
        assert!(fetched.load(Ordering::SeqCst) <= 2 * RANGE_CHUNK);
        let mut all = vec![0u8; data.len()];
        dev.read_at(0, &mut all).unwrap();
        assert!(all == data);
        assert!(dev.write_at(0, b"x").is_err());
    }
}
