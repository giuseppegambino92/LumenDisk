//! Linux / BSD ISO mode (Rufus "ISO mode"): copy the ISO's files onto a fresh FAT32
//! partition and make it bootable — UEFI from the ISO's own `EFI/BOOT` loaders (or the files
//! of its El Torito EFI image), BIOS through SYSLINUX 6.03 when the ISO boots with ISOLINUX 6 —
//! and optionally add a persistence partition (Ubuntu casper, Debian / Kali live-boot).
//!
//! Owner: the ISO / Linux workstream.
//!
//! Layout: `BootTarget::Both` (default) and `Bios` → MBR, FAT32 partition 1 active, SYSLINUX's
//! `mbr.bin` as boot code; `Uefi` → GPT. Persistence → partition 2, ext4, labelled as the live
//! system expects (`writable` / `casper-rw` / `persistence`).
//!
//! The FAT32 label is the ISO's volume id when it fits (11 characters, upper case); otherwise
//! the label references in the boot configurations are rewritten (as Rufus does) so that
//! distributions that find their medium by label (`root=live:CDLABEL=`, `archisolabel=`,
//! GRUB `search --label`) still boot.
//!
//! Not supported (a clear error recommends DD mode): files over 4 GiB (FAT32), BIOS boot of
//! GRUB-only ISOs (no ISOLINUX), ISOLINUX older than 6.x.

pub mod cfg;
pub mod syslinux;

use std::collections::HashSet;
use std::sync::Arc;

use serde_json::{json, Value};

use super::job::{JobCtx, Phase};
use super::layout::{self, NewPart, PartType, TableOptions};
use super::plan::{BootTarget, FlashRequest, Scheme};
use super::target::OpenTarget;
use super::tree::{self, CopyAction, SourceTree, TreeEntry};
use super::{format, ForgeError, ForgeResult, MkfsOptions, TargetFs};
use crate::block::{BlockDev, PartitionDev};
use crate::i18n::t;
use crate::vfs::{self, FileSystem, FsError, Ino, MountDefaults};

/// What [`detect`] found in an ISO (serialised into the inspect result).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LinuxInfo {
    pub distro: String,
    pub has_efi: bool,
    pub has_isolinux: bool,
    pub has_grub: bool,
    /// "casper" (Ubuntu), "live-boot" (Debian), … — None: no persistence support known.
    pub persistence: Option<String>,
    /// The ISO's volume label.
    #[serde(default)]
    pub label: String,
    /// Partition label the persistence partition needs ("writable", "casper-rw", "persistence").
    #[serde(default)]
    pub persistence_label: Option<String>,
    /// ISOLINUX version found in the ISO ("6.04 6.04-pre1").
    #[serde(default)]
    pub syslinux_version: Option<String>,
    /// UEFI loaders in /EFI/BOOT.
    #[serde(default)]
    pub efi_loaders: Vec<String>,
    /// Whether ISO mode can make the medium boot on BIOS PCs.
    #[serde(default)]
    pub bios_iso_mode: bool,
    /// Why not (and what to do instead), when it can't.
    #[serde(default)]
    pub bios_note: Option<String>,
}

/// Largest file FAT32 can store.
const FAT32_MAX: u64 = u32::MAX as u64;
const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Persist {
    /// Ubuntu casper; `old`: releases before 19.10 use the label `casper-rw`.
    Casper { old: bool },
    /// Debian / Kali live-boot.
    LiveBoot,
}

impl Persist {
    fn name(self) -> &'static str {
        match self {
            Persist::Casper { .. } => "casper",
            Persist::LiveBoot => "live-boot",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Persist::Casper { old: true } => "casper-rw",
            Persist::Casper { old: false } => "writable",
            Persist::LiveBoot => "persistence",
        }
    }
    fn params(self) -> cfg::PersistParams {
        match self {
            Persist::Casper { .. } => cfg::PersistParams { param: "persistent", boot_token: "boot=casper", kernel_dir: "/casper/" },
            Persist::LiveBoot => cfg::PersistParams { param: "persistence", boot_token: "boot=live", kernel_dir: "/live/" },
        }
    }
}

/// Where ISOLINUX lives in the ISO.
#[derive(Clone, Debug)]
struct Isolinux {
    /// Directory of isolinux.bin ("/isolinux", "/boot/syslinux").
    dir: String,
    /// Its configuration file (isolinux.cfg, else syslinux.cfg, in that directory).
    cfg: Option<String>,
    version: Option<(u32, u32, String)>,
}

/// Everything ISO mode needs to know about a source tree.
struct Analysis {
    info: LinuxInfo,
    linux: bool,
    windows: bool,
    entries: Vec<TreeEntry>,
    persist: Option<Persist>,
    isolinux: Option<Isolinux>,
}

fn find_ci<'a>(entries: &'a [TreeEntry], path: &str) -> Option<&'a TreeEntry> {
    tree::find(entries, path)
}

fn read_small(tree: &mut dyn SourceTree, e: &TreeEntry, max: u64) -> ForgeResult<Vec<u8>> {
    let n = e.size.min(max) as usize;
    let mut buf = vec![0u8; n];
    let mut off = 0usize;
    while off < n {
        let k = tree.read(&e.path, off as u64, &mut buf[off..])?;
        if k == 0 {
            break;
        }
        off += k;
    }
    buf.truncate(off);
    Ok(buf)
}

fn parent_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// "Ubuntu 18.04.6 LTS …" → releases before 19.10 use `casper-rw`.
fn casper_is_old(info: &str) -> bool {
    for w in info.split_whitespace() {
        let mut it = w.split('.');
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            if let (Ok(maj), Ok(min)) = (a.parse::<u32>(), b.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse::<u32>()) {
                if (4..=40).contains(&maj) {
                    return maj < 19 || (maj == 19 && min < 10);
                }
            }
        }
    }
    false
}

fn analyze(tree: &mut dyn SourceTree, bios_image: Option<&str>) -> ForgeResult<Analysis> {
    let entries = tree.entries()?;
    let label = tree.label();
    let has = |p: &str| find_ci(&entries, p).is_some();
    let windows = has("/bootmgr") || has("/sources/install.wim") || has("/sources/install.esd") || has("/sources/boot.wim");
    let markers = ["/casper", "/live", "/arch", "/LiveOS", "/isolinux", "/boot/isolinux", "/boot/grub", "/boot/grub2", "/EFI/BOOT", "/syslinux", "/boot/syslinux", "/images/pxeboot", "/.disk/info", "/.alpine-release", "/boot/kernel"];
    let linux = !windows && markers.iter().any(|m| has(m));
    let disk_info = match find_ci(&entries, "/.disk/info").cloned() {
        Some(e) => String::from_utf8_lossy(&read_small(tree, &e, 4096)?).lines().next().unwrap_or("").trim().to_string(),
        None => String::new(),
    };
    let distro = if !disk_info.is_empty() {
        disk_info.clone()
    } else if let Some(e) = find_ci(&entries, "/.alpine-release").cloned() {
        format!("Alpine Linux {}", String::from_utf8_lossy(&read_small(tree, &e, 256)?).trim())
    } else if has("/arch") {
        "Arch Linux".into()
    } else if has("/LiveOS") {
        format!("Fedora-based live system ({label})")
    } else if has("/casper") {
        "Ubuntu-based live system".into()
    } else if has("/live") {
        "Debian-based live system".into()
    } else if has("/boot/kernel") {
        "FreeBSD-based system".into()
    } else {
        label.clone()
    };
    let persist = if has("/casper") {
        Some(Persist::Casper { old: casper_is_old(&disk_info) })
    } else if entries.iter().any(|e| !e.is_dir && e.parent().eq_ignore_ascii_case("/live") && e.name().to_ascii_lowercase().ends_with(".squashfs")) {
        Some(Persist::LiveBoot)
    } else {
        None
    };
    let efi_loaders: Vec<String> = entries.iter().filter(|e| !e.is_dir && e.parent().eq_ignore_ascii_case("/efi/boot") && e.name().to_ascii_lowercase().ends_with(".efi")).map(|e| e.path.clone()).collect();
    let has_grub = has("/boot/grub") || has("/boot/grub2") || entries.iter().any(|e| e.name().eq_ignore_ascii_case("grub.cfg"));
    // ISOLINUX: the El Torito BIOS image when it is an isolinux.bin, else the usual places.
    let bins: Vec<&TreeEntry> = entries.iter().filter(|e| !e.is_dir && e.name().eq_ignore_ascii_case("isolinux.bin")).collect();
    let preferred = ["/isolinux/isolinux.bin", "/boot/isolinux/isolinux.bin", "/boot/syslinux/isolinux.bin", "/syslinux/isolinux.bin"];
    let bin = bios_image
        .and_then(|p| bins.iter().find(|e| e.path.eq_ignore_ascii_case(p)))
        .or_else(|| preferred.iter().find_map(|p| bins.iter().find(|e| e.path.eq_ignore_ascii_case(p))))
        .or_else(|| bins.first())
        .map(|e| (*e).clone());
    let isolinux = match bin {
        Some(b) => {
            let dir = parent_of(&b.path);
            let cfg = ["isolinux.cfg", "syslinux.cfg"].iter().find_map(|n| find_ci(&entries, &join(&dir, n))).map(|e| e.path.clone());
            let data = read_small(tree, &b, 1 << 20)?;
            Some(Isolinux { dir, cfg, version: syslinux::detect_version(&data) })
        }
        None => None,
    };
    let bios_note = match &isolinux {
        None if has_grub => Some(t(
            "questa ISO avvia i PC BIOS solo con GRUB: in modalità ISO si avvierà solo in UEFI. Per i PC BIOS scrivila in modalità DD",
            "this ISO boots BIOS PCs with GRUB only: in ISO mode it will boot on UEFI only. For BIOS PCs write it in DD mode",
        )),
        None => Some(t("nessun boot loader BIOS (ISOLINUX) nella ISO: per i PC BIOS usa la modalità DD", "no BIOS boot loader (ISOLINUX) in the ISO: for BIOS PCs use DD mode")),
        Some(i) if i.cfg.is_none() => Some(t("ISOLINUX senza file di configurazione: per i PC BIOS usa la modalità DD", "ISOLINUX without a configuration file: for BIOS PCs use DD mode")),
        Some(Isolinux { version: Some((6, _, _)), .. }) => None,
        Some(Isolinux { version: Some((maj, min, _)), .. }) => Some(t(
            &format!("la ISO usa ISOLINUX {maj}.{min:02}: LumenDisk installa SYSLINUX 6.03, compatibile solo con ISO basate su SYSLINUX 6. Per i PC BIOS scrivila in modalità DD"),
            &format!("the ISO uses ISOLINUX {maj}.{min:02}: LumenDisk installs SYSLINUX 6.03, which only works with ISOs built on SYSLINUX 6. For BIOS PCs write it in DD mode"),
        )),
        Some(_) => Some(t("versione di ISOLINUX non riconosciuta: per i PC BIOS usa la modalità DD", "unrecognised ISOLINUX version: for BIOS PCs use DD mode")),
    };
    let info = LinuxInfo {
        distro,
        has_efi: !efi_loaders.is_empty(),
        has_isolinux: isolinux.is_some(),
        has_grub,
        persistence: persist.map(|p| p.name().to_string()),
        label,
        persistence_label: persist.map(|p| p.label().to_string()),
        syslinux_version: isolinux.as_ref().and_then(|i| i.version.as_ref().map(|v| v.2.clone())),
        efi_loaders,
        bios_iso_mode: bios_note.is_none(),
        bios_note,
    };
    Ok(Analysis { info, linux, windows, entries, persist, isolinux })
}

/// Recognise a Linux (or BSD) live / install ISO. None: not one (e.g. Windows media).
pub fn detect(tree: &mut dyn SourceTree) -> ForgeResult<Option<LinuxInfo>> {
    let a = analyze(tree, None)?;
    Ok(a.linux.then_some(a.info))
}

/// Files of the ISO's El Torito UEFI image (a small FAT file system), for ISOs that have no
/// /EFI/BOOT directory of their own. Bounded: 256 MiB image, 4096 files.
fn efi_image_files(tree: &crate::iso::IsoTree) -> ForgeResult<Option<Vec<(String, Vec<u8>)>>> {
    use crate::iso::BootPlatform;
    let Some(cat) = tree.boot_catalog() else { return Ok(None) };
    let Some(e) = cat.find(BootPlatform::Efi) else { return Ok(None) };
    let img = cat.read_image(tree.device().as_ref(), e, 256 * MIB)?;
    let dev: Arc<dyn BlockDev> = Arc::new(crate::block::MemDev::from_vec(img));
    let Ok(mut fs) = vfs::open_fs(dev, true, MountDefaults::default()) else { return Ok(None) };
    let mut out = vec![];
    let mut todo: Vec<(Ino, String)> = vec![(fs.root_ino(), String::new())];
    let mut total = 0u64;
    while let Some((ino, path)) = todo.pop() {
        for d in fs.readdir(ino)? {
            if d.name == "." || d.name == ".." {
                continue;
            }
            let p = format!("{path}/{}", d.name);
            match d.kind {
                vfs::FileKind::Dir => todo.push((d.ino, p)),
                vfs::FileKind::File => {
                    let size = fs.getattr(d.ino)?.size;
                    total += size;
                    if out.len() >= 4096 || total > 256 * MIB {
                        return Err(ForgeError::Invalid("El Torito EFI image: too many files".into()));
                    }
                    let mut data = Vec::with_capacity(size as usize);
                    while (data.len() as u64) < size {
                        let chunk = fs.read(d.ino, data.len() as u64, (4 * MIB).min(size - data.len() as u64) as u32)?;
                        if chunk.is_empty() {
                            break;
                        }
                        data.extend_from_slice(&chunk);
                    }
                    out.push((p, data));
                }
                vfs::FileKind::Symlink => {}
            }
        }
    }
    let has_loader = out.iter().any(|(p, _)| p.to_ascii_lowercase().starts_with("/efi/boot/") && p.to_ascii_lowercase().ends_with(".efi"));
    Ok(has_loader.then_some(out))
}

/// Inode of `path` in `fs` (each component matched exactly, then ignoring case).
fn resolve(fs: &mut dyn FileSystem, path: &str) -> ForgeResult<Option<Ino>> {
    let mut ino = fs.root_ino();
    for comp in path.split('/').filter(|c| !c.is_empty()) {
        let found = match fs.lookup(ino, comp) {
            Ok(i) => Some(i),
            Err(FsError::NotFound) => fs.readdir(ino)?.into_iter().find(|d| d.name.eq_ignore_ascii_case(comp)).map(|d| d.ino),
            Err(e) => return Err(e.into()),
        };
        match found {
            Some(i) => ino = i,
            None => return Ok(None),
        }
    }
    Ok(Some(ino))
}

/// Zero the first MiB of a partition (signatures of whatever was there before).
fn clear_start(dev: &dyn BlockDev) -> ForgeResult<()> {
    let n = dev.size().min(MIB) as usize;
    dev.write_at(0, &vec![0u8; n])?;
    Ok(())
}

/// Write an ISO in ISO mode. `src` is the ISO image.
pub fn write(r: &FlashRequest, src: Arc<dyn BlockDev>, target: &OpenTarget, ctx: &JobCtx) -> ForgeResult<Value> {
    ctx.phase(Phase::Preparing, 0);
    let mut iso = crate::iso::open(src)?;
    let bios_image = iso.boot_catalog().and_then(|c| c.find(crate::iso::BootPlatform::Bios)).and_then(|e| iso.path_at(e.lba as u64 * 2048)).map(str::to_string);
    let an = analyze(&mut iso, bios_image.as_deref())?;
    if an.windows {
        return Err(ForgeError::Invalid(t("è una ISO di Windows: usa la modalità Windows", "this is a Windows ISO: use the Windows mode")));
    }
    let opts = r.linux.clone().unwrap_or_default();
    let boot = opts.boot.unwrap_or(BootTarget::Both);
    if opts.fs.is_some_and(|f| f != TargetFs::Fat32) {
        return Err(ForgeError::Unsupported(t("la modalità ISO crea una partizione FAT32 (l'unica avviabile ovunque)", "ISO mode creates a FAT32 partition (the only one every firmware boots from)")));
    }
    if let Some(big) = an.entries.iter().find(|e| !e.is_dir && e.size > FAT32_MAX) {
        return Err(ForgeError::Unsupported(t(
            &format!("{} è più grande di 4 GB e non entra in FAT32: scrivi questa ISO in modalità DD", big.path),
            &format!("{} is larger than 4 GB and does not fit on FAT32: write this ISO in DD mode", big.path),
        )));
    }
    let mut warnings: Vec<String> = vec![];
    // ---- what can boot
    let efi_files = if an.info.efi_loaders.is_empty() { efi_image_files(&iso)? } else { None };
    let uefi_ok = !an.info.efi_loaders.is_empty() || efi_files.is_some();
    let bios_ok = an.info.bios_iso_mode;
    let dd = |it: &str, en: &str| ForgeError::Unsupported(t(it, en));
    match boot {
        BootTarget::Uefi if !uefi_ok => return Err(dd("la ISO non contiene un boot loader UEFI: usa la modalità DD", "the ISO has no UEFI boot loader: use DD mode")),
        BootTarget::Bios if !bios_ok => return Err(ForgeError::Unsupported(an.info.bios_note.clone().unwrap_or_default())),
        BootTarget::Both if !uefi_ok && !bios_ok => {
            return Err(dd("la ISO non ha boot loader utilizzabili in modalità ISO: usa la modalità DD", "the ISO has no boot loader usable in ISO mode: use DD mode"));
        }
        BootTarget::Both if !bios_ok => warnings.push(an.info.bios_note.clone().unwrap_or_default()),
        BootTarget::Both if !uefi_ok => warnings.push(t("la ISO non contiene un boot loader UEFI: si avvierà solo sui PC BIOS", "the ISO has no UEFI boot loader: it will boot on BIOS PCs only")),
        _ => {}
    }
    let install_bios = bios_ok && boot != BootTarget::Uefi;
    // ---- persistence
    let persist = match (opts.persistence_size.filter(|&s| s > 0), an.persist) {
        (Some(_), None) => {
            return Err(ForgeError::Unsupported(t(
                "questa ISO non supporta la persistenza (casper di Ubuntu o live-boot di Debian)",
                "this ISO has no known persistence support (Ubuntu casper or Debian live-boot)",
            )))
        }
        (Some(s), Some(k)) if s < 32 * MIB => return Err(ForgeError::Invalid(format!("persistence: at least 32 MiB ({s} bytes requested), kind {}", k.name()))),
        (Some(s), Some(k)) => Some((s / MIB * MIB, k)),
        _ => None,
    };
    // ---- labels
    let iso_label = iso.label();
    let wanted = opts.label.clone().filter(|l| !l.trim().is_empty()).unwrap_or_else(|| iso_label.clone());
    let mut fat_label = format::sanitize_label(TargetFs::Fat32, &wanted);
    if fat_label.trim().is_empty() {
        fat_label = "LIVE".into();
    }
    // ---- layout
    let scheme = if boot == BootTarget::Uefi { Scheme::Gpt } else { Scheme::Mbr };
    let disk = target.size();
    let need: u64 = an.entries.iter().map(|e| if e.is_dir { 32 * 1024 } else { e.size.div_ceil(32 * 1024) * 32 * 1024 }).sum::<u64>() + 16 * MIB;
    let main_size = match persist {
        Some((p, _)) => Some(disk.saturating_sub(2 * MIB).saturating_sub(p) / MIB * MIB),
        None => None,
    };
    let main_avail = main_size.unwrap_or(disk.saturating_sub(2 * MIB));
    if main_avail < need.max(64 * MIB) {
        return Err(ForgeError::Invalid(t(
            &format!("il disco è troppo piccolo: servono almeno {} MB per i file della ISO", need.max(64 * MIB).div_ceil(MIB)),
            &format!("the disk is too small: the ISO's files need at least {} MB", need.max(64 * MIB).div_ceil(MIB)),
        )));
    }
    let mut parts = vec![NewPart { size: main_size, kind: PartType::Fs(TargetFs::Fat32), name: fat_label.clone(), bootable: scheme == Scheme::Mbr }];
    if let Some((_, k)) = persist {
        parts.push(NewPart { size: None, kind: PartType::Fs(TargetFs::Ext4), name: k.label().into(), bootable: false });
    }
    ctx.phase(Phase::Partitioning, 0);
    let table = TableOptions { mbr_boot_code: install_bios.then(|| syslinux::MBR_BIN.to_vec()), ..Default::default() };
    let placed = layout::write_table(target.dev.as_ref(), scheme, &parts, &table)?;
    ctx.info(format!("{:?} partition table: {} partition(s)", scheme, placed.len()));
    // ---- main file system
    ctx.phase(Phase::Formatting, 0);
    let main: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(target.dev.clone(), placed[0].start, placed[0].size)?);
    clear_start(main.as_ref())?;
    format::mkfs(main.clone(), TargetFs::Fat32, &MkfsOptions { label: fat_label.clone(), ..Default::default() })?;
    // BPB hidden sectors = the partition's offset (BIOS boot code relies on it).
    let lba = (placed[0].start / 512).min(u32::MAX as u64) as u32;
    let mut bs = main.read_vec(0, 512)?;
    bs[28..32].copy_from_slice(&lba.to_le_bytes());
    main.write_at(0, &bs)?;
    let bk = u16::from_le_bytes([bs[50], bs[51]]) as u64;
    if (1..32).contains(&bk) {
        let mut b2 = main.read_vec(bk * 512, 512)?;
        b2[28..32].copy_from_slice(&lba.to_le_bytes());
        main.write_at(bk * 512, &b2)?;
    }
    // ---- copy
    let files = an.entries.iter().filter(|e| !e.is_dir && e.symlink.is_none()).count();
    let total_bytes: u64 = an.entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
    ctx.phase(Phase::Extracting, total_bytes);
    ctx.info(format!("copying {files} files ({total_bytes} bytes) from the {:?} view", iso.view()));
    let mut fs = vfs::open_fs(main.clone(), false, MountDefaults::default())?;
    let copied = tree::copy_tree(&mut iso, fs.as_mut(), ctx, Some(FAT32_MAX), |_| CopyAction::Copy)?;
    // ---- make it boot
    ctx.phase(Phase::Customizing, 0);
    let mut modified: HashSet<String> = HashSet::new();
    let mut patches: Vec<Value> = vec![];
    let mut bootloaders: Vec<String> = vec![];
    if let Some(files) = &efi_files {
        let mut added = 0usize;
        for (p, data) in files {
            if resolve(fs.as_mut(), p)?.is_none() {
                tree::write_file(fs.as_mut(), p, data)?;
                modified.insert(p.to_ascii_lowercase());
                added += 1;
            }
        }
        bootloaders.push(format!("UEFI: {added} files extracted from the ISO's El Torito EFI image"));
    } else if uefi_ok {
        bootloaders.push(format!("UEFI: {} (from the ISO)", an.info.efi_loaders.join(", ")));
    }
    // Boot configurations: label references and persistence parameters.
    let relabel = fat_label != iso_label;
    let configs: Vec<&TreeEntry> = an.entries.iter().filter(|e| !e.is_dir && e.symlink.is_none() && cfg::is_config_path(&e.path) && e.size <= MIB).take(4000).collect();
    for e in configs {
        let Some(bytes) = tree::read_file(fs.as_mut(), &e.path)? else { continue };
        let Ok(text) = String::from_utf8(bytes) else { continue };
        let (mut out, nl) = if relabel { cfg::replace_label(&text, &iso_label, &fat_label) } else { (text.clone(), 0) };
        let np = match persist {
            Some((_, k)) => {
                let (o, n) = cfg::add_persistence(&out, &k.params());
                out = o;
                n
            }
            None => 0,
        };
        if out != text {
            tree::write_file(fs.as_mut(), &e.path, out.as_bytes())?;
            modified.insert(e.path.to_ascii_lowercase());
            patches.push(json!({ "file": e.path, "label_references": nl, "persistence_entries": np }));
        }
    }
    let mut syslinux_dir = None;
    if install_bios {
        let iso_l = an.isolinux.clone().ok_or_else(|| ForgeError::Invalid("isolinux".into()))?;
        let dir = iso_l.dir.clone();
        // syslinux.cfg next to isolinux.cfg (SYSLINUX looks for syslinux.cfg in its install
        // directory first; relative paths keep working since that is also the isolinux dir).
        if let Some(c) = &iso_l.cfg {
            if c.to_ascii_lowercase().ends_with("/isolinux.cfg") {
                let content = tree::read_file(fs.as_mut(), c)?.unwrap_or_default();
                let dest = join(&dir, "syslinux.cfg");
                tree::write_file(fs.as_mut(), &dest, &content)?;
                modified.insert(dest.to_ascii_lowercase());
                patches.push(json!({ "file": dest, "created_from": c }));
            }
        }
        // Modules must match ldlinux.sys: replace the ISO's .c32 by the 6.03 builds.
        let same = iso_l.version.as_ref().is_some_and(|v| (v.0, v.1) == syslinux::VERSION_NUM);
        let mut replaced = 0usize;
        let mut missing = vec![];
        if !same {
            for e in an.entries.iter().filter(|e| !e.is_dir && e.name().to_ascii_lowercase().ends_with(".c32")) {
                match syslinux::module(e.name()) {
                    Some(m) => {
                        tree::write_file(fs.as_mut(), &e.path, m)?;
                        modified.insert(e.path.to_ascii_lowercase());
                        replaced += 1;
                    }
                    None => missing.push(e.path.clone()),
                }
            }
            if !missing.is_empty() {
                warnings.push(format!("no SYSLINUX {} build of {}: kept the ISO's copy", syslinux::VERSION, missing.join(", ")));
            }
        }
        let ldc32 = join(&dir, "ldlinux.c32");
        tree::write_file(fs.as_mut(), &ldc32, syslinux::module("ldlinux.c32").unwrap_or_default())?;
        let ldsys = join(&dir, "ldlinux.sys");
        tree::write_file(fs.as_mut(), &ldsys, &syslinux::ldlinux_file())?;
        modified.insert(ldc32.to_ascii_lowercase());
        modified.insert(ldsys.to_ascii_lowercase());
        let from = iso_l.version.as_ref().map(|v| v.2.clone()).unwrap_or_default();
        bootloaders.push(format!("BIOS: SYSLINUX {} in {dir} (ISO had ISOLINUX {from}; {replaced} modules replaced), MBR boot code", syslinux::VERSION));
        syslinux_dir = Some(dir);
    }
    fs.sync()?;
    drop(fs);
    if let Some(dir) = &syslinux_dir {
        syslinux::install(main.as_ref(), placed[0].start / 512, dir)?;
    }
    // ---- persistence
    let mut persistence = Value::Null;
    if let (Some((_, k)), Some(p)) = (persist, placed.get(1)) {
        ctx.phase(Phase::Formatting, 0);
        let pdev: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(target.dev.clone(), p.start, p.size)?);
        clear_start(pdev.as_ref())?;
        format::mkfs(pdev.clone(), TargetFs::Ext4, &MkfsOptions { label: k.label().into(), ..Default::default() })?;
        if k == Persist::LiveBoot {
            let mut pfs = vfs::open_fs(pdev, false, MountDefaults::default())?;
            tree::write_file(pfs.as_mut(), "/persistence.conf", b"/ union\n")?;
            pfs.sync()?;
        }
        persistence = json!({ "kind": k.name(), "label": k.label(), "size": p.size, "partition": p.index });
        ctx.info(format!("persistence partition {} ({} bytes, ext4, label {})", p.index, p.size, k.label()));
    }
    // ---- verify the copied files
    let mut verified = 0u64;
    if r.verify {
        ctx.phase(Phase::Verifying, copied);
        let mut fs = vfs::open_fs(main.clone(), true, MountDefaults::default())?;
        let mut a = vec![0u8; 4 * MIB as usize];
        for e in an.entries.iter().filter(|e| !e.is_dir && e.symlink.is_none() && !modified.contains(&e.path.to_ascii_lowercase())) {
            let ino = resolve(fs.as_mut(), &e.path)?.ok_or_else(|| ForgeError::Mismatch(format!("{}: missing on the target", e.path)))?;
            let mut off = 0u64;
            while off < e.size {
                let want = (a.len() as u64).min(e.size - off) as usize;
                let n = iso.read(&e.path, off, &mut a[..want])?;
                if n == 0 {
                    break;
                }
                let b = fs.read(ino, off, n as u32)?;
                if b[..] != a[..n] {
                    return Err(ForgeError::Mismatch(format!("{}: differs on the target at byte {off}", e.path)));
                }
                off += n as u64;
                verified += n as u64;
                ctx.advance(n as u64)?;
            }
        }
    }
    ctx.phase(Phase::Finishing, 0);
    target.dev.flush()?;
    let fs_name = |i: usize| if i == 0 { "fat32" } else { "ext4" };
    let partitions: Vec<Value> = placed
        .iter()
        .enumerate()
        .map(|(i, p)| json!({ "index": p.index, "start": p.start, "size": p.size, "fs": fs_name(i), "label": if i == 0 { fat_label.clone() } else { persist.map(|(_, k)| k.label().to_string()).unwrap_or_default() }, "role": if i == 0 { "boot" } else { "persistence" } }))
        .collect();
    for w in &warnings {
        ctx.warn(w.clone());
    }
    Ok(json!({
        "mode": "iso",
        "distro": an.info.distro,
        "iso_label": iso_label,
        "label": fat_label,
        "view": iso.view(),
        "scheme": if scheme == Scheme::Gpt { "gpt" } else { "mbr" },
        "partitions": partitions,
        "files_copied": files,
        "bytes_copied": copied,
        "bytes_verified": verified,
        "bootloaders": bootloaders,
        "patches": patches,
        "persistence": persistence,
        "warnings": warnings,
    }))
}

#[cfg(test)]
mod tests;
