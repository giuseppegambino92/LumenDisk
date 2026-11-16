//! Windows installation media: FAT32 + split WIM, or NTFS + UEFI:NTFS; BIOS boot records;
//! autounattend.xml (hardware-check bypass, local account, unattended OOBE).
//!
//! Owner: the Windows workstream.
//!
//! [`write_tree`] does the whole job from the ISO's files (any [`SourceTree`]):
//! 1. look at the media ([`detect`]): version, architecture, install image, editions;
//! 2. choose the layout:
//!    - **FAT32** (default, boots UEFI and BIOS): every file copied as is, except an
//!      `install.wim` over 4 GiB − 1, which is split into `install.swm`, `install2.swm`… of at
//!      most 3800 MiB each, like `Dism /Split-Image /FileSize:3800` ([`wim`]);
//!    - **NTFS + UEFI:NTFS** (Rufus's layout) when asked, or when the media can't go on FAT32:
//!      an `install.esd` (solid) or another file over 4 GiB. Partition 1 NTFS with every file,
//!      partition 2 the 1 MiB UEFI:NTFS FAT image at the end of the disk ([`uefi_ntfs`]);
//! 3. partition (MBR with the Windows 7 boot code for BIOS / both, GPT for UEFI only),
//!    format, copy, write the parts, write `autounattend.xml` ([`unattend`]), install the NT6
//!    partition boot record that loads BOOTMGR ([`bootrec`]), and read everything back.

pub mod bootrec;
mod ms_sys;
pub mod uefi_ntfs;
pub mod unattend;
pub mod wim;

use std::sync::Arc;

use serde_json::{json, Value};

use super::job::{JobCtx, Phase};
use super::layout::{self, NewPart, PartType, Placed, TableOptions};
use super::plan::{BootTarget, FlashRequest, Scheme, WindowsLayout, WindowsOptions};
use super::target::OpenTarget;
use super::tree::{self, CopyAction, SourceTree, TreeEntry};
use super::{ForgeError, ForgeResult, MkfsOptions, TargetFs};
use crate::block::{BlockDev, PartitionDev};
use crate::i18n::t;
use crate::vfs::{self, FileSystem, FsError, Ino, MountDefaults};
use unattend::WinArch;

/// Largest file FAT32 can hold.
pub const FAT32_MAX_FILE: u64 = u32::MAX as u64;
/// Volume label when the user gives none: what Microsoft's Media Creation Tool uses.
pub const DEFAULT_LABEL: &str = "ESD-USB";

/// One edition (image) of the install image.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Edition {
    pub index: u32,
    pub name: String,
    pub edition_id: String,
    pub arch: Option<String>,
    pub build: Option<u32>,
}

/// What [`detect`] found in an ISO (serialised into the inspect result).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct WindowsInfo {
    /// "Windows 11", "Windows 10", "Windows 8.1", "Windows 7", "Windows Server 2022"…
    pub version: String,
    /// "x64", "arm64", "x86".
    pub arch: String,
    /// `/sources/install.wim` or `/sources/install.esd`.
    pub install_image: String,
    pub install_image_size: u64,
    pub editions: Vec<String>,
    /// Build number (26100…) when known.
    pub build: Option<u32>,
    pub server: bool,
    pub editions_detail: Vec<Edition>,
    /// Languages of the install image, default first.
    pub languages: Vec<String>,
    /// "wim", "esd", "swm" (already split), "" (no install image: Windows PE media).
    pub image_format: String,
    /// "none", "xpress", "lzx", "lzms".
    pub compression: String,
    /// The install image is larger than FAT32 allows (4 GiB − 1).
    pub needs_split: bool,
    /// When `needs_split`: why it can't be split (solid ESD…), None if it can.
    pub split_blocker: Option<String>,
    /// BOOTMGR present: the media boots in BIOS mode.
    pub bios_boot: bool,
    /// UEFI loaders found in `/efi/boot` ("x64", "arm64", "x86").
    pub uefi_boot: Vec<String>,
    /// The layout [`write_tree`] picks when the user doesn't choose.
    pub recommended_layout: Option<WindowsLayout>,
}

/// Knobs the tests turn (the defaults are the real limits).
#[derive(Clone, Debug)]
pub struct Tuning {
    /// Files above this don't go on FAT32 (4 GiB − 1).
    pub max_file: u64,
    /// Maximum size of each `.swm` part.
    pub part_limit: u64,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { max_file: FAT32_MAX_FILE, part_limit: wim::DEFAULT_PART_LIMIT }
    }
}

/// The install image as found on the media (its header and XML, when it is a WIM).
struct InstallImage {
    wim: Option<wim::WimFile>,
}

struct Media {
    info: WindowsInfo,
    install: Option<InstallImage>,
    /// Files other than the install image that FAT32 can't hold.
    big_files: Vec<String>,
}

impl Media {
    fn unattend_media(&self) -> unattend::Media {
        let arch = match self.info.arch.as_str() {
            "x64" => Some(WinArch::X64),
            "arm64" => Some(WinArch::Arm64),
            "x86" => Some(WinArch::X86),
            _ => None,
        };
        unattend::Media { arch, build: self.info.build, server: self.info.server, languages: self.info.languages.clone() }
    }
}

fn find_file<'a>(entries: &'a [TreeEntry], path: &str) -> Option<&'a TreeEntry> {
    tree::find(entries, path).filter(|e| !e.is_dir)
}

/// Windows' name for a build (client or server), unless an edition name says it plainly.
pub fn version_name(build: Option<u32>, server: bool, names: &[String]) -> String {
    for n in names {
        if let Some(i) = n.find("Windows Server ") {
            let rest = &n[i + "Windows Server ".len()..];
            let year: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if year.len() == 4 {
                let r2 = if rest[4..].trim_start().starts_with("R2") { " R2" } else { "" };
                return format!("Windows Server {year}{r2}");
            }
        }
        for v in ["Windows 11", "Windows 10", "Windows 8.1", "Windows 7"] {
            if n.contains(v) && !server {
                return v.to_string();
            }
        }
    }
    let Some(b) = build else { return if server { "Windows Server".into() } else { "Windows".into() } };
    if server {
        match b {
            26100.. => "Windows Server 2025",
            20348..=26099 => "Windows Server 2022",
            17763..=20347 => "Windows Server 2019",
            14393..=17762 => "Windows Server 2016",
            9600..=14392 => "Windows Server 2012 R2",
            9200..=9599 => "Windows Server 2012",
            7600..=9199 => "Windows Server 2008 R2",
            6001..=7599 => "Windows Server 2008",
            _ => "Windows Server",
        }
        .into()
    } else {
        match b {
            22000.. => "Windows 11",
            10240..=21999 => "Windows 10",
            9600..=10239 => "Windows 8.1",
            9200..=9599 => "Windows 8",
            7600..=9199 => "Windows 7",
            6000..=7599 => "Windows Vista",
            _ => "Windows",
        }
        .into()
    }
}

/// `sources/idwbinfo.txt`: `BuildArch=amd64`, `BuildFull=26100.1.amd64fre…`.
fn idwbinfo(tree: &mut dyn SourceTree, entries: &[TreeEntry]) -> (Option<WinArch>, Option<u32>) {
    let Some(e) = find_file(entries, "/sources/idwbinfo.txt").filter(|e| e.size <= 64 * 1024) else { return (None, None) };
    let mut buf = vec![0u8; e.size as usize];
    if wim::read_exact(tree, &e.path, 0, &mut buf).is_err() {
        return (None, None);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut arch = None;
    let mut build = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k.trim().to_ascii_lowercase().as_str() {
            "buildarch" => {
                arch = match v.trim().to_ascii_lowercase().as_str() {
                    "amd64" | "x64" => Some(WinArch::X64),
                    "arm64" => Some(WinArch::Arm64),
                    "x86" | "i386" => Some(WinArch::X86),
                    _ => None,
                }
            }
            "buildfull" => build = v.trim().split('.').next().and_then(|n| n.parse().ok()),
            _ => {}
        }
    }
    (arch, build)
}

/// Look at the media (see [`WindowsInfo`]). Only the WIM header, its XML and, for an image
/// that must be split, its blob table are read.
fn analyse(tree: &mut dyn SourceTree, entries: &[TreeEntry], tune: &Tuning) -> ForgeResult<Option<Media>> {
    let install = ["/sources/install.wim", "/sources/install.esd", "/sources/install.swm"].iter().find_map(|p| find_file(entries, p)).cloned();
    let boot_wim = find_file(entries, "/sources/boot.wim").cloned();
    let bios_boot = find_file(entries, "/bootmgr").is_some();
    let uefi_boot: Vec<String> = [("bootx64.efi", "x64"), ("bootaa64.efi", "arm64"), ("bootia32.efi", "x86")]
        .iter()
        .filter(|(f, _)| find_file(entries, &format!("/efi/boot/{f}")).is_some())
        .map(|(_, a)| a.to_string())
        .collect();
    if install.is_none() && boot_wim.is_none() {
        return Ok(None);
    }
    if !bios_boot && uefi_boot.is_empty() && boot_wim.is_none() {
        return Ok(None);
    }
    let mut info = WindowsInfo { bios_boot, uefi_boot: uefi_boot.clone(), ..Default::default() };
    // The image whose XML describes the media: install image, else boot.wim (Windows PE).
    let meta_src = install.clone().or(boot_wim);
    let mut image = None;
    if let Some(e) = &install {
        info.install_image = e.path.clone();
        info.install_image_size = e.size;
        info.image_format = e.path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    }
    let mut wim_meta = None;
    if let Some(e) = &meta_src {
        let w = wim::WimFile::open(tree, &e.path, e.size)?;
        info.compression = w.header.compression().to_string();
        wim_meta = Some(w);
    }
    if let Some(w) = &wim_meta {
        for img in &w.images {
            info.editions.push(img.name.clone());
            info.editions_detail.push(Edition {
                index: img.index,
                name: img.name.clone(),
                edition_id: img.edition_id.clone(),
                arch: img.arch.and_then(WinArch::from_wim).map(|a| a.label().to_string()),
                build: img.build,
            });
        }
        let first = w.images.iter().find(|i| i.build.is_some());
        info.build = first.and_then(|i| i.build);
        info.server = w.images.iter().any(|i| i.installation_type.to_ascii_lowercase().contains("server") || i.edition_id.to_ascii_lowercase().starts_with("server"));
        let mut langs: Vec<String> = vec![];
        for img in &w.images {
            for l in img.default_language.iter().chain(img.languages.iter()) {
                if !langs.iter().any(|x| x.eq_ignore_ascii_case(l)) {
                    langs.push(l.clone());
                }
            }
        }
        info.languages = langs;
        if let Some(a) = w.images.iter().find_map(|i| i.arch.and_then(WinArch::from_wim)) {
            info.arch = a.label().to_string();
        }
    }
    let (id_arch, id_build) = idwbinfo(tree, entries);
    if info.build.is_none() {
        info.build = id_build;
    }
    if info.arch.is_empty() {
        info.arch = id_arch.map(|a| a.label().to_string()).or_else(|| uefi_boot.first().cloned()).unwrap_or_default();
    }
    info.version = version_name(info.build, info.server, &info.editions);

    if let Some(e) = &install {
        let w = if Some(&e.path) == meta_src.as_ref().map(|m| &m.path) { wim_meta.take() } else { None };
        info.needs_split = e.size > tune.max_file && info.image_format != "swm";
        if info.needs_split {
            if let Some(w) = &w {
                let table = w.blob_table(tree)?;
                info.split_blocker = wim::plan_split(w, &table, tune.part_limit).err().map(|e| e.to_string());
            } else {
                info.split_blocker = Some("not a WIM".into());
            }
        }
        image = Some(InstallImage { wim: w });
    }
    let install_path = install.as_ref().map(|e| e.path.clone());
    let big_files: Vec<String> = entries.iter().filter(|e| !e.is_dir && e.size > tune.max_file && Some(&e.path) != install_path.as_ref()).map(|e| e.path.clone()).collect();
    info.recommended_layout = Some(if big_files.is_empty() && (!info.needs_split || info.split_blocker.is_none()) { WindowsLayout::Fat32Split } else { WindowsLayout::NtfsUefi });
    Ok(Some(Media { info, install: image, big_files }))
}

/// Recognise Windows installation media (None: not Windows).
pub fn detect(tree: &mut dyn SourceTree) -> ForgeResult<Option<WindowsInfo>> {
    let entries = tree.entries()?;
    Ok(analyse(tree, &entries, &Tuning::default())?.map(|m| m.info))
}

/// Write Windows media from an ISO.
pub fn write(r: &FlashRequest, src: Arc<dyn BlockDev>, target: &OpenTarget, ctx: &JobCtx) -> ForgeResult<Value> {
    let mut tree = open_iso(src)?;
    write_tree(r, tree.as_mut(), target, ctx)
}

/// HOOKUP(iso): the ISO 9660 / UDF reader belongs to another workstream. Once `crate::iso`
/// exists, the body of this function becomes `crate::iso::open(src)` (the ISO as a
/// `Box<dyn SourceTree>`); everything else already works on any [`SourceTree`].
fn open_iso(_src: Arc<dyn BlockDev>) -> ForgeResult<Box<dyn SourceTree>> {
    Err(ForgeError::Unsupported(t("modalità Windows: il lettore ISO non è ancora disponibile", "Windows mode: the ISO reader is not available yet")))
}

/// [`write_tree_tuned`] with the real limits.
pub fn write_tree(r: &FlashRequest, tree: &mut dyn SourceTree, target: &OpenTarget, ctx: &JobCtx) -> ForgeResult<Value> {
    write_tree_tuned(r, tree, target, ctx, &Tuning::default())
}

/// What goes on the disk.
enum Plan {
    Fat32 { split: Option<(wim::WimFile, Vec<wim::BlobEntry>, wim::SplitPlan)> },
    Ntfs,
}

fn choose(opts: &WindowsOptions, media: &mut Media, tree: &mut dyn SourceTree, tune: &Tuning, ctx: &JobCtx) -> ForgeResult<Plan> {
    let info = &media.info;
    let fat32_refusal = if let Some(f) = media.big_files.first() {
        Some(t(&format!("{f} è più grande di 4 GB e non entra in FAT32"), &format!("{f} is larger than 4 GB and does not fit on FAT32")))
    } else if info.needs_split {
        info.split_blocker.clone().map(|why| {
            t(
                &format!("{} è più grande di 4 GB e non si può dividere ({why})", info.install_image),
                &format!("{} is larger than 4 GB and can't be split ({why})", info.install_image),
            )
        })
    } else {
        None
    };
    match (opts.layout, fat32_refusal) {
        (Some(WindowsLayout::NtfsUefi), _) => Ok(Plan::Ntfs),
        (Some(WindowsLayout::Fat32Split), Some(why)) => Err(ForgeError::Unsupported(t(&format!("{why}: scegli NTFS + UEFI:NTFS"), &format!("{why}: choose NTFS + UEFI:NTFS")))),
        (None, Some(why)) => {
            ctx.info(format!("NTFS + UEFI:NTFS layout: {why}"));
            Ok(Plan::Ntfs)
        }
        (Some(WindowsLayout::Fat32Split) | None, None) => {
            if !info.needs_split {
                return Ok(Plan::Fat32 { split: None });
            }
            let img = media.install.as_mut().ok_or_else(|| ForgeError::Invalid("no install image".into()))?;
            let w = img.wim.take().ok_or_else(|| ForgeError::Invalid("install image is not a WIM".into()))?;
            let table = w.blob_table(tree)?;
            let plan = wim::plan_split(&w, &table, tune.part_limit)?;
            Ok(Plan::Fat32 { split: Some((w, table, plan)) })
        }
    }
}

/// A [`SourceTree`] without one file (the install image that is written as split parts), so
/// that `copy_tree`'s byte total is right.
struct Without<'a> {
    inner: &'a mut dyn SourceTree,
    hidden: Option<String>,
}

impl SourceTree for Without<'_> {
    fn entries(&mut self) -> ForgeResult<Vec<TreeEntry>> {
        let mut e = self.inner.entries()?;
        if let Some(h) = &self.hidden {
            e.retain(|x| !x.path.eq_ignore_ascii_case(h));
        }
        Ok(e)
    }
    fn read(&mut self, path: &str, offset: u64, buf: &mut [u8]) -> ForgeResult<usize> {
        self.inner.read(path, offset, buf)
    }
    fn label(&self) -> String {
        self.inner.label()
    }
}

fn fs_err(path: &str, e: FsError) -> ForgeError {
    match e {
        FsError::NoSpace => ForgeError::Io(t(&format!("spazio esaurito scrivendo {path}"), &format!("out of space writing {path}"))),
        other => ForgeError::Io(format!("{path}: {other}")),
    }
}

/// Writes `.swm` parts into a directory of the target volume.
struct FsSink<'a> {
    fs: &'a mut dyn FileSystem,
    dir: Ino,
    dir_path: String,
    cur: Option<(Ino, u64, String)>,
}

impl wim::PartSink for FsSink<'_> {
    fn begin(&mut self, _part: u16, name: &str, _size: u64) -> ForgeResult<()> {
        let path = format!("{}/{name}", self.dir_path);
        match self.fs.lookup(self.dir, name) {
            Ok(_) => self.fs.unlink(self.dir, name).map_err(|e| fs_err(&path, e))?,
            Err(FsError::NotFound) => {}
            Err(e) => return Err(fs_err(&path, e)),
        }
        let ino = self.fs.create(self.dir, name, 0o644, 0, 0).map_err(|e| fs_err(&path, e))?;
        self.cur = Some((ino, 0, path));
        Ok(())
    }
    fn append(&mut self, data: &[u8]) -> ForgeResult<()> {
        let (ino, off, path) = self.cur.as_mut().ok_or_else(|| ForgeError::Io("no part open".into()))?;
        let mut w = 0usize;
        while w < data.len() {
            let n = self.fs.write(*ino, *off, &data[w..]).map_err(|e| fs_err(path, e))? as usize;
            if n == 0 {
                return Err(ForgeError::Io(format!("{path}: short write")));
            }
            w += n;
            *off += n as u64;
        }
        Ok(())
    }
    fn end(&mut self) -> ForgeResult<()> {
        self.cur = None;
        Ok(())
    }
}

fn lookup_path(fs: &mut dyn FileSystem, path: &str) -> ForgeResult<Option<Ino>> {
    let mut ino = fs.root_ino();
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        ino = match fs.lookup(ino, comp) {
            Ok(i) => i,
            Err(FsError::NotFound) => return Ok(None),
            Err(e) => return Err(fs_err(path, e)),
        };
    }
    Ok(Some(ino))
}

/// Compare `size` bytes of `path` on the volume with `expected` chunks.
fn compare_file(fs: &mut dyn FileSystem, path: &str, size: u64, ctx: &JobCtx, mut expected: impl FnMut(u64, &mut [u8]) -> ForgeResult<()>) -> ForgeResult<()> {
    const CHUNK: u64 = 4 << 20;
    let mismatch = |what: &str| ForgeError::Mismatch(t(&format!("verifica fallita: {path} ({what})"), &format!("verification failed: {path} ({what})")));
    let ino = lookup_path(fs, path)?.ok_or_else(|| mismatch("missing"))?;
    if fs.getattr(ino).map_err(|e| fs_err(path, e))?.size != size {
        return Err(mismatch("size"));
    }
    let mut want = vec![0u8; CHUNK as usize];
    let mut off = 0u64;
    while off < size {
        let n = CHUNK.min(size - off);
        let got = fs.read(ino, off, n as u32).map_err(|e| fs_err(path, e))?;
        expected(off, &mut want[..n as usize])?;
        if got.len() as u64 != n || got[..] != want[..n as usize] {
            return Err(mismatch(&format!("content at {off}")));
        }
        off += n;
        ctx.advance(n)?;
    }
    Ok(())
}

/// Do the whole job (see the module documentation) from `tree` onto `target`.
pub fn write_tree_tuned(r: &FlashRequest, tree: &mut dyn SourceTree, target: &OpenTarget, ctx: &JobCtx, tune: &Tuning) -> ForgeResult<Value> {
    ctx.phase(Phase::Preparing, 0);
    let entries = tree.entries()?;
    let mut media = analyse(tree, &entries, tune)?.ok_or_else(|| ForgeError::Invalid(t("non è un'immagine di installazione di Windows", "not a Windows installation image")))?;
    let info = media.info.clone();
    ctx.info(format!(
        "{} {} ({}), install image {} ({} bytes, {}), {} edition(s)",
        info.version,
        info.arch,
        info.build.map(|b| b.to_string()).unwrap_or_else(|| "build ?".into()),
        if info.install_image.is_empty() { "-" } else { &info.install_image },
        info.install_image_size,
        info.compression,
        info.editions.len()
    ));
    let opts = r.windows.clone().unwrap_or_default();
    let boot = opts.boot.unwrap_or(BootTarget::Both);
    let bios = boot != BootTarget::Uefi;
    if bios && !info.bios_boot {
        ctx.warn("no BOOTMGR on the media: it will not boot in BIOS mode");
    }
    if boot != BootTarget::Bios && info.uefi_boot.is_empty() {
        ctx.warn("no /efi/boot/boot*.efi on the media: it will not boot in UEFI mode");
    }
    // Everything that can fail on the user's input fails before the disk is touched.
    let answer = unattend::build(&opts, &media.unattend_media())?;
    if let Some(a) = &answer {
        for n in &a.notes {
            ctx.info(format!("autounattend: {n}"));
        }
    }
    let plan = choose(&opts, &mut media, tree, tune, ctx)?;
    let (layout_kind, fs_kind) = match plan {
        Plan::Fat32 { .. } => (WindowsLayout::Fat32Split, TargetFs::Fat32),
        Plan::Ntfs => (WindowsLayout::NtfsUefi, TargetFs::Ntfs),
    };
    let label_in = opts.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| DEFAULT_LABEL.to_string());
    let label = super::format::sanitize_label(fs_kind, &label_in);

    // Room check: data, cluster slack, filesystem metadata (FATs / MFT, ~1.5 % of the disk).
    // An estimate: copying stops with "out of space" anyway if it was too optimistic.
    let files: Vec<&TreeEntry> = entries.iter().filter(|e| !e.is_dir).collect();
    let data: u64 = files.iter().map(|e| e.size).sum();
    let disk = target.dev.size() / 512 * 512;
    let need = data.saturating_add(files.len() as u64 * 4096).saturating_add(disk / 64).saturating_add(8 << 20);
    if need > disk {
        return Err(ForgeError::Invalid(t(
            &format!("il disco è troppo piccolo: servono {:.1} GB, ne ha {:.1}", need as f64 / 1e9, disk as f64 / 1e9),
            &format!("the disk is too small: {:.1} GB needed, it has {:.1}", need as f64 / 1e9, disk as f64 / 1e9),
        )));
    }

    // ── Partitioning
    ctx.check()?;
    ctx.phase(Phase::Partitioning, 0);
    let scheme = if bios { Scheme::Mbr } else { Scheme::Gpt };
    const MIB: u64 = 1 << 20;
    let mut parts = vec![NewPart { size: None, kind: PartType::Fs(fs_kind), name: label_in.clone(), bootable: bios }];
    if layout_kind == WindowsLayout::NtfsUefi {
        let last_end = if scheme == Scheme::Gpt { disk.saturating_sub(16384 + 512) } else { disk };
        let uefi_start = last_end.saturating_sub(uefi_ntfs::PARTITION_SIZE) / MIB * MIB;
        let main = uefi_start.saturating_sub(MIB);
        if main < 64 * MIB {
            return Err(ForgeError::Invalid(t("il disco è troppo piccolo", "the disk is too small")));
        }
        parts[0].size = Some(main);
        parts.push(NewPart {
            size: Some(uefi_ntfs::PARTITION_SIZE),
            kind: PartType::Raw { mbr: uefi_ntfs::MBR_TYPE, gpt: layout::guid(uefi_ntfs::GPT_TYPE) },
            name: uefi_ntfs::GPT_NAME.into(),
            bootable: false,
        });
    }
    let topts = TableOptions { mbr_boot_code: bios.then(bootrec::win7_mbr), ..Default::default() };
    let placed: Vec<Placed> = layout::write_table(target.dev.as_ref(), scheme, &parts, &topts)?;
    if layout_kind == WindowsLayout::NtfsUefi && scheme == Scheme::Gpt {
        uefi_ntfs::gpt_add_attributes(target.dev.as_ref(), 2, uefi_ntfs::GPT_ATTRIBUTES)?;
    }
    ctx.info(format!("{scheme:?} partition table: {} partition(s){}", placed.len(), if bios { ", Windows 7 MBR boot code" } else { "" }));
    ctx.check()?;

    // ── Formatting
    ctx.phase(Phase::Formatting, 0);
    let main = placed[0];
    let pdev: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(target.dev.clone(), main.start, main.size)?);
    super::format::mkfs(pdev.clone(), fs_kind, &MkfsOptions { label: label.clone(), ..Default::default() })?;
    ctx.info(format!("{} \"{label}\" created ({} bytes)", fs_kind.display_name(), main.size));
    ctx.check()?;

    // ── Extracting the files
    let mut volume = vfs::open_fs(pdev.clone(), false, MountDefaults::default())?;
    ctx.phase(Phase::Extracting, 0);
    let split = match &plan {
        Plan::Fat32 { split: Some(s) } => Some(s),
        _ => None,
    };
    let hidden = split.map(|(w, _, _)| w.path.clone());
    let max_file = (fs_kind == TargetFs::Fat32).then_some(tune.max_file);
    let copied = {
        let mut view = Without { inner: &mut *tree, hidden: hidden.clone() };
        tree::copy_tree(&mut view, volume.as_mut(), ctx, max_file, |_| CopyAction::Copy)?
    };
    ctx.info(format!("{copied} bytes copied"));

    // ── Writing the split install image
    let mut written_parts = vec![];
    if let Some((w, table, sp)) = split {
        ctx.phase(Phase::Writing, sp.data_bytes(table));
        let (dir, file) = w.path.rsplit_once('/').unwrap_or(("", &w.path));
        let dir = if dir.is_empty() { "/" } else { dir };
        let base = file.rsplit_once('.').map(|(b, _)| b).unwrap_or(file).to_string();
        let dir_ino = tree::mkdir_p(volume.as_mut(), dir)?;
        let mut sink = FsSink { fs: volume.as_mut(), dir: dir_ino, dir_path: dir.trim_end_matches('/').to_string(), cur: None };
        written_parts = wim::write_split(tree, w, table, sp, &base, &mut sink, ctx)?;
        ctx.info(format!("{} split into {} part(s) of at most {} bytes", w.path, written_parts.len(), tune.part_limit));
    }

    // ── Customizing: answer file, boot records, UEFI:NTFS
    ctx.phase_units(Phase::Customizing, 3, "steps");
    if let Some(a) = &answer {
        tree::write_file(volume.as_mut(), "/autounattend.xml", a.xml.as_bytes())?;
        ctx.info(if a.custom { "custom autounattend.xml written" } else { "autounattend.xml written" });
    }
    volume.sync()?;
    drop(volume);
    ctx.advance(1)?;
    let start_lba = main.start / 512;
    if bios {
        match fs_kind {
            TargetFs::Fat32 => bootrec::install_fat32(pdev.as_ref(), start_lba)?,
            _ => bootrec::install_ntfs(pdev.as_ref(), start_lba)?,
        }
        ctx.info(format!("NT6 {} boot record installed (loads BOOTMGR)", fs_kind.display_name()));
    }
    ctx.advance(1)?;
    let uefi_dev = if layout_kind == WindowsLayout::NtfsUefi {
        let p = placed[1];
        let d: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(target.dev.clone(), p.start, p.size)?);
        uefi_ntfs::write(d.as_ref())?;
        ctx.info(format!("{} written to partition 2", uefi_ntfs::VERSION));
        Some(d)
    } else {
        None
    };
    target.dev.flush()?;
    ctx.advance(1)?;

    // ── Verifying
    if r.verify {
        let total = data.saturating_sub(hidden.as_ref().map_or(0, |_| info.install_image_size)) + written_parts.iter().map(|p| p.size).sum::<u64>();
        ctx.phase(Phase::Verifying, total);
        let mut volume = vfs::open_fs(pdev.clone(), true, MountDefaults::default())?;
        for e in &files {
            if e.symlink.is_some() || hidden.as_ref().is_some_and(|h| h.eq_ignore_ascii_case(&e.path)) {
                continue;
            }
            let path = e.path.clone();
            compare_file(volume.as_mut(), &e.path, e.size, ctx, |off, buf| wim::read_exact(tree, &path, off, buf))?;
        }
        if let Some((w, _, _)) = split {
            let dir = w.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            for p in &written_parts {
                let path = format!("{dir}/{}", p.name);
                let ino = lookup_path(volume.as_mut(), &path)?.ok_or_else(|| ForgeError::Mismatch(format!("{path} missing")))?;
                let mut sha = <sha2::Sha256 as sha2::Digest>::new();
                let mut off = 0u64;
                while off < p.size {
                    let n = (p.size - off).min(4 << 20);
                    let chunk = volume.read(ino, off, n as u32).map_err(|e| fs_err(&path, e))?;
                    if chunk.is_empty() {
                        break;
                    }
                    sha2::Digest::update(&mut sha, &chunk);
                    off += chunk.len() as u64;
                    ctx.advance(chunk.len() as u64)?;
                }
                let got: [u8; 32] = sha2::Digest::finalize(sha).into();
                if off != p.size || got != p.sha256 {
                    return Err(ForgeError::Mismatch(t(&format!("verifica fallita: {path}"), &format!("verification failed: {path}"))));
                }
            }
        }
        if let Some(a) = &answer {
            if tree::read_file(volume.as_mut(), "/autounattend.xml")?.as_deref() != Some(a.xml.as_bytes()) {
                return Err(ForgeError::Mismatch("verification failed: /autounattend.xml".into()));
            }
        }
        drop(volume);
        let boot_ok = !bios
            || (bootrec::has_win7_mbr(target.dev.as_ref())
                && match fs_kind {
                    TargetFs::Fat32 => bootrec::has_fat32(pdev.as_ref(), start_lba),
                    _ => bootrec::has_ntfs(pdev.as_ref(), start_lba),
                });
        if !boot_ok || uefi_dev.as_ref().is_some_and(|d| !uefi_ntfs::present(d.as_ref())) {
            return Err(ForgeError::Mismatch(t("verifica fallita: record di avvio", "verification failed: boot records")));
        }
        ctx.info("verified");
    }

    Ok(json!({
        "mode": "windows",
        "windows": info,
        "layout": layout_kind,
        "boot": boot,
        "scheme": scheme,
        "filesystem": fs_kind.display_name(),
        "label": label,
        "partitions": placed,
        "files": files.len(),
        "bytes_copied": copied,
        "split": split.map(|(w, _, _)| json!({
            "source": w.path,
            "part_limit": tune.part_limit,
            "parts": written_parts.iter().map(|p| json!({"name": p.name, "size": p.size})).collect::<Vec<_>>(),
        })),
        "autounattend": answer.as_ref().map(|a| json!({"custom": a.custom, "notes": a.notes})),
        "boot_records": bios.then(|| json!({"mbr": "windows7", "pbr": format!("nt6-{}", fs_kind.display_name().to_ascii_lowercase())})),
        "uefi_ntfs": (layout_kind == WindowsLayout::NtfsUefi).then_some(uefi_ntfs::VERSION),
        "verified": r.verify,
    }))
}

#[cfg(test)]
mod tests;
