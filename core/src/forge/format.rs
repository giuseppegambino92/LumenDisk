//! One [`FormatRequest`] → partition table + filesystems.
//!
//! Owner: the partitioning workstream (layouts, cluster sizes, label rules, orchestration).
//!
//! [`run`] validates everything it can *before* writing a byte: the request itself
//! ([`validate_request`]), the placement of the partitions on the actual disk, and each
//! filesystem on its partition ([`preflight`]: a dry run of the real `mkfs` against a sparse
//! in-memory overlay, so size / cluster-size combinations a filesystem can't represent are
//! refused with the disk untouched). Only then it clears old signatures, writes the table and
//! formats the partitions one by one, checking for cancellation between steps.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use super::job::{JobCtx, Phase};
use super::layout::{self, NewPart, PartType, TableOptions};
use super::plan::{FormatRequest, PartSpec, Scheme};
use super::{ForgeError, ForgeResult, MkfsOptions, TargetFs};
use crate::block::{BlockDev, PartitionDev};
use crate::i18n::t;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;

/// Create filesystem `fs` on `dev` (a whole partition / device view).
pub fn mkfs(dev: Arc<dyn BlockDev>, fs: TargetFs, opts: &MkfsOptions) -> ForgeResult<()> {
    mkfs_unit(dev, fs, opts).map(|_| ())
}

/// [`mkfs`], returning the allocation unit actually used (cluster / block size in bytes) for
/// the filesystems whose drivers report it.
pub fn mkfs_unit(dev: Arc<dyn BlockDev>, fs: TargetFs, opts: &MkfsOptions) -> ForgeResult<Option<u32>> {
    use crate::vfs::{self, FsType};
    let label = sanitize_label(fs, &opts.label);
    let c = opts.cluster_size;
    let unit = match fs {
        TargetFs::Ext2 => vfs::format_with(dev, FsType::Ext2, &label, c)?,
        TargetFs::Ext3 => vfs::format_with(dev, FsType::Ext3, &label, c)?,
        TargetFs::Ext4 => vfs::format_with(dev, FsType::Ext4, &label, c)?,
        TargetFs::Fat16 => vfs::format_with(dev, FsType::Fat16, &label, c)?,
        TargetFs::Fat32 => vfs::format_with(dev, FsType::Fat32, &label, c)?,
        TargetFs::Exfat => vfs::format_with(dev, FsType::ExFat, &label, c)?,
        TargetFs::Ntfs => vfs::format_with(dev, FsType::Ntfs, &label, c)?,
        TargetFs::Btrfs => return crate::mkfs::btrfs::format(dev, &MkfsOptions { label, ..opts.clone() }).map(|_| None).map_err(Into::into),
        TargetFs::Xfs => return crate::mkfs::xfs::format(dev, &MkfsOptions { label, ..opts.clone() }).map(|_| None).map_err(Into::into),
        TargetFs::F2fs => return crate::mkfs::f2fs::format(dev, &MkfsOptions { label, ..opts.clone() }).map(|_| None).map_err(Into::into),
        TargetFs::Ufs2 => return crate::mkfs::ufs::format(dev, &MkfsOptions { label, ..opts.clone() }).map(|_| None).map_err(Into::into),
    };
    Ok(Some(unit))
}

/// The label as `fs` can store it (length, character set, case). Lengths are counted in the
/// unit each filesystem stores: bytes for FAT (11, upper-case ASCII without the characters
/// the FAT specification forbids), ext (16 bytes of UTF-8), XFS, Btrfs, UFS2; UTF-16 code
/// units for exFAT (11, exFAT spec §7.3.2) and NTFS (32, the Windows limit) and F2FS.
pub fn sanitize_label(fs: TargetFs, label: &str) -> String {
    fn utf8(c: char) -> usize {
        c.len_utf8()
    }
    fn utf16(c: char) -> usize {
        c.len_utf16()
    }
    let (max, unit): (usize, fn(char) -> usize) = match fs {
        TargetFs::Fat16 | TargetFs::Fat32 => (11, utf8),
        TargetFs::Exfat => (11, utf16),
        TargetFs::Ntfs => (32, utf16),
        TargetFs::Ext2 | TargetFs::Ext3 | TargetFs::Ext4 => (16, utf8),
        TargetFs::Xfs => (12, utf8),
        TargetFs::Btrfs => (255, utf8),
        TargetFs::F2fs => (512, utf16),
        TargetFs::Ufs2 => (32, utf8),
    };
    let mut out: String = label.chars().filter(|c| !c.is_control()).collect();
    if matches!(fs, TargetFs::Fat16 | TargetFs::Fat32) {
        out = out.to_uppercase().chars().filter(|c| c.is_ascii() && !"\"*+,./:;<=>?[\\]|".contains(*c)).collect();
    }
    let mut s = String::new();
    let mut used = 0;
    for c in out.chars() {
        if used + unit(c) > max {
            break;
        }
        used += unit(c);
        s.push(c);
    }
    if matches!(fs, TargetFs::Fat16 | TargetFs::Fat32) {
        // FAT pads labels with spaces: trailing ones would be lost anyway.
        s.truncate(s.trim_end().len());
    }
    s
}

/// The allocation units a filesystem accepts, as (min, max) in bytes (powers of two), or the
/// explicit list for ext. None: not validated here (filesystems of other workstreams).
///
/// - FAT16 / FAT32: 512 B – 64 KiB. One byte holds the sectors per cluster (≤ 128 with
///   512-byte sectors); 64 KiB is outside Microsoft's recommendation for FAT32 but accepted
///   by Windows NT, Linux and macOS. The cluster *count* must also fit the FAT type
///   (FAT16 4085–65524, FAT32 ≥ 65525): checked against the partition size in [`preflight`].
/// - exFAT: 512 B – 32 MiB (exFAT spec §3.1.15: SectorsPerClusterShift ≤ 25 − BytesPerSectorShift).
/// - NTFS: 512 B – 2 MiB (Windows 10 1709+; ntfs-3g since 2022.5.17 and Linux ntfs3 mount
///   them; the boot sector stores cluster sizes above 64 KiB as a negative power of two).
/// - ext2/3/4: 1024, 2048 or 4096 bytes (larger blocks than the page size don't mount on
///   x86 / most ARM Linux).
pub fn cluster_limits(fs: TargetFs) -> Option<(u64, u64)> {
    match fs {
        TargetFs::Fat16 | TargetFs::Fat32 => Some((512, 64 * KIB)),
        TargetFs::Exfat => Some((512, 32 * MIB)),
        TargetFs::Ntfs => Some((512, 2 * MIB)),
        TargetFs::Ext2 | TargetFs::Ext3 | TargetFs::Ext4 => Some((1024, 4096)),
        _ => None,
    }
}

fn human(n: u64) -> String {
    if n >= GIB && n % GIB == 0 {
        format!("{} GiB", n / GIB)
    } else if n >= MIB && n % MIB == 0 {
        format!("{} MiB", n / MIB)
    } else if n >= KIB && n % KIB == 0 {
        format!("{} KiB", n / KIB)
    } else {
        format!("{n} B")
    }
}

/// Checks that need no disk: partition count per scheme, cluster sizes per filesystem.
pub fn validate_request(r: &FormatRequest) -> ForgeResult<()> {
    if r.partitions.is_empty() {
        return Err(ForgeError::Invalid(t("nessuna partizione richiesta", "no partition requested")));
    }
    match r.scheme {
        Scheme::None if r.partitions.len() != 1 => {
            return Err(ForgeError::Invalid(t(
                "senza tabella delle partizioni si può creare un solo filesystem",
                "without a partition table there can be exactly one filesystem",
            )))
        }
        Scheme::Mbr if r.partitions.len() > 4 => {
            return Err(ForgeError::Invalid(t("MBR: al massimo 4 partizioni (usa GPT per averne di più)", "MBR: at most 4 partitions (use GPT for more)")))
        }
        _ => {}
    }
    for (i, p) in r.partitions.iter().enumerate() {
        let n = i + 1;
        if p.size.is_none() && n != r.partitions.len() {
            return Err(ForgeError::Invalid(t("solo l'ultima partizione può occupare il resto del disco", "only the last partition can take the rest of the disk")));
        }
        let (Some(c), Some((lo, hi))) = (p.mkfs.cluster_size, cluster_limits(p.fs)) else { continue };
        let c = c as u64;
        let ext = matches!(p.fs, TargetFs::Ext2 | TargetFs::Ext3 | TargetFs::Ext4);
        if !c.is_power_of_two() || c < lo || c > hi {
            let fs = p.fs.display_name();
            return Err(ForgeError::Invalid(if ext {
                t(
                    &format!("partizione {n}: {fs} accetta blocchi da 1024, 2048 o 4096 byte, non {c}"),
                    &format!("partition {n}: {fs} takes 1024, 2048 or 4096-byte blocks, not {c}"),
                )
            } else {
                t(
                    &format!("partizione {n}: dimensione del cluster {c} non valida per {fs} (potenza di 2 da {} a {})", human(lo), human(hi)),
                    &format!("partition {n}: invalid cluster size {c} for {fs} (a power of two from {} to {})", human(lo), human(hi)),
                )
            }));
        }
    }
    Ok(())
}

/// Write-discarding device for dry runs: what a `mkfs` writes is kept (except all-zero
/// chunks never written before, which read back as zeros anyway) so drivers that read their
/// own metadata back work; reads elsewhere return zeros. Gives up beyond `budget` bytes of
/// stored data (`Err(Other)`), so a dry run never costs more than that much memory.
struct DryRun {
    size: u64,
    sector: u32,
    chunks: Mutex<HashMap<u64, Vec<u8>>>,
    budget: usize,
}

const DRY_CHUNK: u64 = 64 * KIB;

impl DryRun {
    fn new(size: u64, sector: u32) -> Self {
        DryRun { size, sector, chunks: Mutex::new(HashMap::new()), budget: 256 * MIB as usize }
    }
    fn range(&self, off: u64, len: usize) -> std::io::Result<()> {
        if off.checked_add(len as u64).map_or(true, |e| e > self.size) {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "dry run: I/O beyond the end"));
        }
        Ok(())
    }
}

impl BlockDev for DryRun {
    fn read_at(&self, off: u64, buf: &mut [u8]) -> std::io::Result<()> {
        self.range(off, buf.len())?;
        let chunks = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let (idx, o) = (pos / DRY_CHUNK, (pos % DRY_CHUNK) as usize);
            let n = (DRY_CHUNK as usize - o).min(buf.len() - done);
            match chunks.get(&idx) {
                Some(c) => buf[done..done + n].copy_from_slice(&c[o..o + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn write_at(&self, off: u64, buf: &[u8]) -> std::io::Result<()> {
        self.range(off, buf.len())?;
        let mut chunks = self.chunks.lock().unwrap_or_else(|e| e.into_inner());
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let (idx, o) = (pos / DRY_CHUNK, (pos % DRY_CHUNK) as usize);
            let n = (DRY_CHUNK as usize - o).min(buf.len() - done);
            let src = &buf[done..done + n];
            if let Some(c) = chunks.get_mut(&idx) {
                c[o..o + n].copy_from_slice(src);
            } else if src.iter().any(|&b| b != 0) {
                if (chunks.len() + 1) * DRY_CHUNK as usize > self.budget {
                    return Err(std::io::Error::other("dry run budget exceeded"));
                }
                let mut c = vec![0u8; DRY_CHUNK as usize];
                c[o..o + n].copy_from_slice(src);
                chunks.insert(idx, c);
            }
            done += n;
        }
        Ok(())
    }
    fn size(&self) -> u64 {
        self.size
    }
    fn flush(&self) -> std::io::Result<()> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        false
    }
    fn logical_sector_size(&self) -> Option<u32> {
        Some(self.sector)
    }
}

/// Can `fs` be created on a partition of `size` bytes on a disk with `sector`-byte logical
/// sectors, with these options? The rules everybody asks about get their own message; the
/// rest is a dry run of the real `mkfs`. Ok(None) also when the dry run was inconclusive
/// (too much metadata to simulate): the real run will tell.
pub fn preflight(index: usize, fs: TargetFs, size: u64, sector: u64, opts: &MkfsOptions) -> ForgeResult<()> {
    let name = fs.display_name();
    let refuse = |it: String, en: String| Err(ForgeError::Invalid(t(&format!("partizione {index}: {it}"), &format!("partition {index}: {en}"))));
    let cluster = opts.cluster_size.map(u64::from);
    match fs {
        TargetFs::Fat16 if size > 4 * GIB => {
            return refuse(
                "FAT16 non può superare 4 GiB (2 GiB con i cluster standard da 32 KiB): scegli FAT32 o exFAT".into(),
                "FAT16 can't exceed 4 GiB (2 GiB with standard 32 KiB clusters): choose FAT32 or exFAT".into(),
            )
        }
        TargetFs::Fat16 if size > 2 * GIB && cluster.is_none() => {
            return refuse(
                "FAT16 oltre 2 GiB richiede cluster da 64 KiB (poco compatibili): sceglili esplicitamente, o usa FAT32 / exFAT".into(),
                "FAT16 above 2 GiB needs 64 KiB clusters (poorly supported): choose them explicitly, or use FAT32 / exFAT".into(),
            )
        }
        TargetFs::Fat32 if size / sector > u32::MAX as u64 => {
            let max = human(sector << 32);
            return refuse(
                format!("FAT32 può usare al massimo {max} su questo disco (settori da {sector} byte): scegli exFAT o una partizione più piccola"),
                format!("FAT32 can use at most {max} on this disk ({sector}-byte sectors): choose exFAT or a smaller partition"),
            );
        }
        TargetFs::Ext2 | TargetFs::Ext3 | TargetFs::Ext4 => {
            let bs = cluster.unwrap_or_else(|| crate::vfs::ext4_fs::default_block_size(size).max(sector));
            if bs < sector {
                return refuse(
                    format!("blocchi da {bs} byte più piccoli dei settori del disco ({sector} byte)"),
                    format!("{bs}-byte blocks are smaller than the disk's {sector}-byte sectors"),
                );
            }
            if size < 8 * MIB {
                return refuse(format!("{name} richiede almeno 8 MiB"), format!("{name} needs at least 8 MiB"));
            }
            if size / bs > u32::MAX as u64 {
                return refuse(
                    format!("troppo grande per {name} con blocchi da {bs} byte (massimo {})", human(bs << 32)),
                    format!("too large for {name} with {bs}-byte blocks (at most {})", human(bs << 32)),
                );
            }
            // The ext driver reads back what it wrote while building the root directory and
            // the journal: the rules above are what it can refuse.
            return Ok(());
        }
        TargetFs::Ntfs if sector > 512 => {
            return refuse(
                format!("NTFS su dischi con settori da {sector} byte (4K nativi) non è ancora supportato: usa exFAT"),
                format!("NTFS on disks with {sector}-byte (4K-native) sectors is not supported yet: use exFAT"),
            )
        }
        _ => {}
    }
    if let (Some(c), true) = (cluster, matches!(fs, TargetFs::Fat16 | TargetFs::Fat32 | TargetFs::Exfat)) {
        if c < sector {
            return refuse(
                format!("cluster da {c} byte più piccoli dei settori del disco ({sector} byte)"),
                format!("{c}-byte clusters are smaller than the disk's {sector}-byte sectors"),
            );
        }
    }
    let dry: Arc<dyn BlockDev> = Arc::new(DryRun::new(size, sector as u32));
    match mkfs_unit(dry, fs, opts) {
        Ok(_) => Ok(()),
        Err(ForgeError::Io(e)) if e.contains("dry run budget") => Ok(()),
        Err(ForgeError::Invalid(e)) => {
            let what = cluster.map(|c| format!("{name}, cluster {}", human(c))).unwrap_or_else(|| name.to_string());
            let on = human_size(size);
            // The usual reasons, in the user's language; the driver's own words otherwise.
            if let (Some(c), Some((min, max))) = (cluster, fat_cluster_counts(fs)) {
                let n = size / c;
                if n < min {
                    return refuse(
                        format!("con cluster da {} su {on} ci sono circa {n} cluster, ma {name} ne richiede almeno {min}: scegli cluster più piccoli", human(c)),
                        format!("{} clusters on {on} make about {n} clusters, but {name} needs at least {min}: choose smaller clusters", human(c)),
                    );
                }
                if n > max {
                    return refuse(
                        format!("con cluster da {} su {on} ci sarebbero circa {n} cluster, più dei {max} ammessi da {name}: scegli cluster più grandi", human(c)),
                        format!("{} clusters on {on} would make about {n} clusters, more than the {max} {name} allows: choose larger clusters", human(c)),
                    );
                }
            }
            if e.contains("too small") {
                return refuse(format!("{on} sono troppo pochi per {what}"), format!("{on} is too small for {what}"));
            }
            refuse(format!("impossibile creare {what} su {on}: {e}"), format!("can't create {what} on {on}: {e}"))
        }
        Err(ForgeError::Unsupported(e)) => Err(ForgeError::Unsupported(t(
            &format!("partizione {index}: {name} non è ancora disponibile ({e})"),
            &format!("partition {index}: {name} is not available yet ({e})"),
        ))),
        Err(e) => Err(e),
    }
}

/// Valid cluster counts for the FAT types (see `fat_fs::cluster_count_range`).
fn fat_cluster_counts(fs: TargetFs) -> Option<(u64, u64)> {
    use crate::vfs::{fat_fs, FsType};
    match fs {
        TargetFs::Fat16 => Some(fat_fs::cluster_count_range(FsType::Fat16)),
        TargetFs::Fat32 => Some(fat_fs::cluster_count_range(FsType::Fat32)),
        _ => None,
    }
}

fn human_size(n: u64) -> String {
    if n >= GIB {
        format!("{:.1} GiB", n as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", n as f64 / MIB as f64)
    }
}

fn part_kind(p: &PartSpec) -> PartType {
    PartType::Fs(p.fs)
}

/// Zero `len` bytes at `off` (chunked).
fn zero(dev: &dyn BlockDev, off: u64, len: u64) -> ForgeResult<()> {
    let buf = vec![0u8; MIB as usize];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(MIB);
        dev.write_at(off + done, &buf[..n as usize])?;
        done += n;
    }
    Ok(())
}

pub fn run(r: &FormatRequest, ctx: &JobCtx) -> ForgeResult<Value> {
    ctx.phase(Phase::Preparing, 0);
    validate_request(r)?;
    let target = super::target::open_for_write(&r.target, ctx)?;
    format_device(target.dev.clone(), &target.path, r, ctx)
}

/// The body of [`run`] once the target is open (also used by tests on RAM disks).
pub fn format_device(dev: Arc<dyn BlockDev>, path: &str, r: &FormatRequest, ctx: &JobCtx) -> ForgeResult<Value> {
    ctx.phase(Phase::Preparing, 0);
    validate_request(r)?;
    let sector = layout::sector_size(dev.as_ref());
    let size = dev.size() / sector * sector;
    let parts: Vec<NewPart> = r
        .partitions
        .iter()
        .map(|p| NewPart { size: p.size, kind: part_kind(p), name: p.name.clone().unwrap_or_else(|| p.mkfs.label.clone()), bootable: false })
        .collect();
    let placed = layout::place(size, sector, r.scheme, &parts, MIB)?;
    for (p, pl) in r.partitions.iter().zip(&placed) {
        ctx.check()?;
        preflight(pl.index as usize, p.fs, pl.size, sector, &p.mkfs)?;
    }
    ctx.check()?;
    ctx.info(format!(
        "{path}: {} bytes, {sector}-byte sectors, {} with {} partition(s)",
        size,
        match r.scheme {
            Scheme::Gpt => "GPT",
            Scheme::Mbr => "MBR",
            Scheme::Apm => "APM",
            Scheme::None => "no partition table",
        },
        placed.len()
    ));

    // Partitioning: clear the old signatures, write the table, clear each partition's start.
    ctx.phase_units(Phase::Partitioning, 3, "steps");
    if r.clear_signatures {
        let n = MIB.min(size);
        zero(dev.as_ref(), 0, n)?;
        zero(dev.as_ref(), size - n, n)?;
    }
    ctx.advance(1)?;
    let written = layout::write_table(dev.as_ref(), r.scheme, &parts, &TableOptions::default())?;
    if written != placed {
        return Err(ForgeError::Io("partition placement changed while writing the table".into()));
    }
    ctx.advance(1)?;
    if r.clear_signatures {
        for pl in &placed {
            zero(dev.as_ref(), pl.start, MIB.min(pl.size))?;
        }
    }
    dev.flush()?;
    ctx.advance(1)?;

    // Formatting.
    ctx.phase_units(Phase::Formatting, placed.len() as u64, "partitions");
    let mut out = vec![];
    for (p, pl) in r.partitions.iter().zip(&placed) {
        ctx.check()?;
        let label = sanitize_label(p.fs, &p.mkfs.label);
        ctx.info(format!("partition {}: {} \"{}\" at {} ({} bytes)", pl.index, p.fs.display_name(), label, pl.start, pl.size));
        let view: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), pl.start, pl.size)?);
        let unit = mkfs_unit(view, p.fs, &p.mkfs).inspect_err(|_| {
            if pl.index > 1 {
                ctx.warn(format!("partitions 1..{} were formatted, partition {} failed", pl.index - 1, pl.index));
            }
        })?;
        out.push(json!({
            "index": pl.index,
            "start": pl.start,
            "size": pl.size,
            "fs": p.fs,
            "label": label,
            "name": parts[pl.index as usize - 1].name,
            "cluster_size": unit,
        }));
        ctx.advance(1)?;
    }
    ctx.phase(Phase::Finishing, 0);
    dev.flush()?;
    Ok(json!({
        "target": path,
        "scheme": r.scheme,
        "sector_size": sector,
        "disk_size": size,
        "partitions": out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{parse_partitions, MemDev};
    use crate::forge::layout::tests::SectorDev;
    use crate::forge::target::TargetSpec;
    use crate::vfs;

    fn spec(fs: TargetFs, size: Option<u64>, label: &str, cluster: Option<u32>) -> PartSpec {
        PartSpec { fs, mkfs: MkfsOptions { label: label.into(), cluster_size: cluster, uuid: None }, size, name: None }
    }

    fn req(scheme: Scheme, parts: Vec<PartSpec>) -> FormatRequest {
        FormatRequest { target: TargetSpec::File { path: "/nonexistent".into(), size: None }, scheme, partitions: parts, clear_signatures: true }
    }

    fn fmt(dev: Arc<dyn BlockDev>, r: &FormatRequest) -> ForgeResult<Value> {
        format_device(dev, "mem", r, &JobCtx::detached(false))
    }

    #[test]
    fn labels_follow_each_filesystems_rules() {
        assert_eq!(sanitize_label(TargetFs::Fat32, "Dati: 2024/è ok"), "DATI 2024 O");
        assert_eq!(sanitize_label(TargetFs::Fat32, "ab        c"), "AB        C");
        assert_eq!(sanitize_label(TargetFs::Fat32, "abc    "), "ABC");
        assert_eq!(sanitize_label(TargetFs::Exfat, "Città di Torino"), "Città di To", "11 UTF-16 units, not bytes");
        assert_eq!(sanitize_label(TargetFs::Exfat, "1234567890😀"), "1234567890", "no half surrogate pair");
        assert_eq!(sanitize_label(TargetFs::Ntfs, &"é".repeat(40)).chars().count(), 32);
        assert_eq!(sanitize_label(TargetFs::Ext4, "ééééééééé"), "éééééééé", "16 bytes of UTF-8, whole characters");
        assert_eq!(sanitize_label(TargetFs::Ext4, "a\nb"), "ab");
    }

    #[test]
    fn requests_are_validated_before_touching_the_disk() {
        let bad = [
            req(Scheme::Gpt, vec![]),
            req(Scheme::None, vec![spec(TargetFs::Fat32, Some(64 << 20), "", None), spec(TargetFs::Fat32, None, "", None)]),
            req(Scheme::Mbr, (0..5).map(|_| spec(TargetFs::Fat32, Some(64 << 20), "", None)).collect()),
            req(Scheme::Gpt, vec![spec(TargetFs::Fat32, None, "", None), spec(TargetFs::Fat32, None, "", None)]),
            req(Scheme::Gpt, vec![spec(TargetFs::Fat32, None, "", Some(3000))]),
            req(Scheme::Gpt, vec![spec(TargetFs::Fat32, None, "", Some(128 << 10))]),
            req(Scheme::Gpt, vec![spec(TargetFs::Exfat, None, "", Some(64 << 20))]),
            req(Scheme::Gpt, vec![spec(TargetFs::Ntfs, None, "", Some(4 << 20))]),
            req(Scheme::Gpt, vec![spec(TargetFs::Ntfs, None, "", Some(256))]),
            req(Scheme::Gpt, vec![spec(TargetFs::Ext4, None, "", Some(8192))]),
            req(Scheme::Gpt, vec![spec(TargetFs::Ext4, None, "", Some(512))]),
        ];
        for r in &bad {
            assert!(matches!(validate_request(r), Err(ForgeError::Invalid(_))), "{r:?}");
            // …and the disk is not written.
            let m = Arc::new(MemDev::new(64 << 20));
            m.write_at(0, &[0x5A; 512]).unwrap();
            assert!(fmt(m.clone(), r).is_err());
            assert_eq!(m.snapshot()[..512], [0x5A; 512]);
        }
        let ok = req(Scheme::Gpt, vec![spec(TargetFs::Exfat, None, "", Some(32 << 20)), spec(TargetFs::Ext4, None, "", Some(2048))]);
        assert!(validate_request(&ok).is_err(), "rest-of-disk partition not last");
        let ok = req(Scheme::Gpt, vec![spec(TargetFs::Exfat, Some(1 << 30), "", Some(32 << 20)), spec(TargetFs::Ext4, None, "", Some(2048))]);
        assert!(validate_request(&ok).is_ok());
    }

    #[test]
    fn size_dependent_combinations_are_refused_with_the_disk_untouched() {
        let cases: Vec<(u64, u64, TargetFs, Option<u32>)> = vec![
            (5 * GIB, 512, TargetFs::Fat16, Some(65536)),
            (3 * GIB, 512, TargetFs::Fat16, None),
            (3 << 40, 512, TargetFs::Fat32, None),
            (20 * MIB, 512, TargetFs::Fat32, None),
            (1 * GIB, 512, TargetFs::Fat32, Some(65536)),
            (8 * MIB, 512, TargetFs::Fat16, Some(65536)),
            (64 * MIB, 512, TargetFs::Exfat, Some(32 << 20)),
            (6 * MIB, 512, TargetFs::Ext4, None),
            (64 * MIB, 4096, TargetFs::Ext4, Some(1024)),
            (64 * MIB, 4096, TargetFs::Ntfs, None),
            (64 * MIB, 4096, TargetFs::Fat32, Some(2048)),
            (8 * MIB, 512, TargetFs::Ntfs, Some(2 << 20)),
        ];
        for (size, sector, fs, c) in cases {
            let e = preflight(1, fs, size, sector, &MkfsOptions { cluster_size: c, ..Default::default() });
            assert!(matches!(e, Err(ForgeError::Invalid(_))), "{fs:?} {size} {sector} {c:?}: {e:?}");
            assert!(e.unwrap_err().to_string().contains('1'));
        }
        // Valid ones, incl. FAT32 well beyond 32 GB (Windows won't create it, but it is valid)
        // and FAT16 up to 4 GiB with 64 KiB clusters.
        let fine: Vec<(u64, u64, TargetFs, Option<u32>)> = vec![
            (64 * GIB, 512, TargetFs::Fat32, None),
            ((2 << 40) - 4096, 512, TargetFs::Fat32, None),
            (4 * GIB - 64 * MIB, 512, TargetFs::Fat16, Some(65536)),
            (64 * MIB, 512, TargetFs::Fat32, Some(512)),
            (512 * MIB, 4096, TargetFs::Fat32, None),
            (512 * MIB, 512, TargetFs::Exfat, Some(32 << 20)),
            (64 * MIB, 512, TargetFs::Ntfs, Some(512)),
            (256 * MIB, 512, TargetFs::Ntfs, Some(2 << 20)),
            (64 * MIB, 512, TargetFs::Ext4, Some(2048)),
        ];
        for (size, sector, fs, c) in fine {
            let e = preflight(1, fs, size, sector, &MkfsOptions { cluster_size: c, ..Default::default() });
            assert!(e.is_ok(), "{fs:?} {size} {sector} {c:?}: {e:?}");
        }
    }

    #[test]
    fn unsupported_filesystems_fail_before_writing() {
        for fs in [TargetFs::Btrfs, TargetFs::Xfs, TargetFs::F2fs, TargetFs::Ufs2] {
            let m = Arc::new(MemDev::new(64 << 20));
            m.write_at(0, &[0x5A; 512]).unwrap();
            match fmt(m.clone(), &req(Scheme::Gpt, vec![spec(fs, None, "x", None)])) {
                // Still a stub: refused cleanly with the disk untouched.
                Err(ForgeError::Unsupported(e)) => {
                    assert!(e.contains(fs.display_name()), "{e}");
                    assert_eq!(m.snapshot()[..512], [0x5A; 512]);
                }
                // Implemented by its workstream meanwhile.
                Ok(v) => assert_eq!(v["partitions"][0]["fs"], serde_json::to_value(fs).unwrap()),
                Err(e) => panic!("{fs:?}: {e}"),
            }
        }
    }

    #[test]
    fn every_scheme_and_filesystem_formats_and_mounts_back() {
        let all = [TargetFs::Fat16, TargetFs::Fat32, TargetFs::Exfat, TargetFs::Ntfs, TargetFs::Ext2, TargetFs::Ext3, TargetFs::Ext4];
        for scheme in [Scheme::Gpt, Scheme::Mbr, Scheme::Apm, Scheme::None] {
            for fs in all {
                let m: Arc<dyn BlockDev> = Arc::new(MemDev::new(80 << 20));
                let r = req(scheme, vec![spec(fs, None, "Lumen", None)]);
                let v = fmt(m.clone(), &r).unwrap_or_else(|e| panic!("{scheme:?} {fs:?}: {e}"));
                let p = &v["partitions"][0];
                let (start, size) = (p["start"].as_u64().unwrap(), p["size"].as_u64().unwrap());
                assert_eq!(start, if scheme == Scheme::None { 0 } else { 1 << 20 });
                let (got_scheme, tp) = layout::read_table(m.as_ref()).unwrap();
                assert_eq!(got_scheme, scheme);
                if scheme != Scheme::None {
                    assert_eq!((tp[0].start, tp[0].size), (start, size));
                }
                let view: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(m.clone(), start, size).unwrap());
                let want = fs.mountable().unwrap();
                assert_eq!(vfs::probe(view.as_ref()), Some(want), "{scheme:?} {fs:?}");
                let f = vfs::open_fs(view, false, crate::vfs::fat_fs::tests::defaults()).unwrap();
                let label = f.volume_info().label;
                assert!(label.eq_ignore_ascii_case("Lumen"), "{scheme:?} {fs:?}: {label:?}");
            }
        }
    }

    #[test]
    fn cluster_sizes_reach_the_filesystems() {
        let cases: Vec<(TargetFs, u32)> = vec![
            (TargetFs::Fat32, 512),
            (TargetFs::Fat32, 2048),
            (TargetFs::Fat16, 2048),
            (TargetFs::Fat16, 65536),
            (TargetFs::Exfat, 512),
            (TargetFs::Exfat, 1 << 20),
            (TargetFs::Ntfs, 512),
            (TargetFs::Ntfs, 8192),
            (TargetFs::Ntfs, 65536),
            (TargetFs::Ntfs, 2 << 20),
            (TargetFs::Ext4, 1024),
            (TargetFs::Ext4, 2048),
            (TargetFs::Ext2, 4096),
        ];
        for (fs, c) in cases {
            let size = if (fs, c) == (TargetFs::Fat16, 2048) { 100 << 20 } else { 300 << 20 };
            let m: Arc<dyn BlockDev> = Arc::new(MemDev::new(size));
            let v = fmt(m.clone(), &req(Scheme::Gpt, vec![spec(fs, None, "C", Some(c))])).unwrap_or_else(|e| panic!("{fs:?} {c}: {e}"));
            assert_eq!(v["partitions"][0]["cluster_size"], c, "{fs:?}");
            let p = &v["partitions"][0];
            let view: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(m.clone(), p["start"].as_u64().unwrap(), p["size"].as_u64().unwrap()).unwrap());
            let mut f = vfs::open_fs(view, false, crate::vfs::fat_fs::tests::defaults()).unwrap_or_else(|e| panic!("{fs:?} {c}: {e:?}"));
            assert_eq!(f.statfs().unwrap().block_size, c, "{fs:?}");
        }
    }

    #[test]
    fn fat_and_exfat_use_4096_byte_sectors_on_4kn_disks() {
        for fs in [TargetFs::Fat32, TargetFs::Exfat, TargetFs::Ext4] {
            let m = Arc::new(SectorDev::new(300 << 20, 4096));
            let dev: Arc<dyn BlockDev> = m.clone();
            let v = fmt(dev.clone(), &req(Scheme::Gpt, vec![spec(fs, None, "K", None)])).unwrap();
            let p = &v["partitions"][0];
            assert_eq!(v["sector_size"], 4096);
            let start = p["start"].as_u64().unwrap();
            let boot = dev.read_vec(start, 512).unwrap();
            match fs {
                TargetFs::Fat32 => assert_eq!(u16::from_le_bytes([boot[11], boot[12]]), 4096),
                TargetFs::Exfat => assert_eq!(boot[108], 12),
                _ => {}
            }
            let view: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), start, p["size"].as_u64().unwrap()).unwrap());
            assert_eq!(vfs::probe(view.as_ref()), fs.mountable());
            assert!(vfs::open_fs(view, false, crate::vfs::fat_fs::tests::defaults()).is_ok());
            assert_eq!(parse_partitions(dev.as_ref()).unwrap()[0].start, start);
        }
    }

    #[test]
    fn several_partitions_and_progress_and_cancel() {
        let m: Arc<dyn BlockDev> = Arc::new(MemDev::new(200 << 20));
        let r = req(
            Scheme::Gpt,
            vec![
                PartSpec { name: Some("EFI".into()), ..spec(TargetFs::Fat32, Some(40 << 20), "EFI", None) },
                spec(TargetFs::Ntfs, Some(60 << 20), "Windows", None),
                spec(TargetFs::Ext4, None, "linux", None),
            ],
        );
        let ctx = JobCtx::detached(false);
        let v = format_device(m.clone(), "mem", &r, &ctx).unwrap();
        let s = ctx.snapshot(0);
        assert_eq!(s.phases, vec![Phase::Preparing, Phase::Partitioning, Phase::Formatting, Phase::Finishing]);
        assert_eq!(v["partitions"].as_array().unwrap().len(), 3);
        let names: Vec<String> = parse_partitions(m.as_ref()).unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["EFI", "Windows", "linux"]);
        // Cancelled before it starts: nothing written.
        let m = Arc::new(MemDev::new(200 << 20));
        m.write_at(0, &[7; 512]).unwrap();
        let ctx = JobCtx::detached(false);
        ctx.cancel();
        assert!(matches!(format_device(m.clone(), "mem", &r, &ctx), Err(ForgeError::Cancelled)));
        assert_eq!(m.snapshot()[..512], [7; 512]);
    }

    #[test]
    fn dry_run_device_keeps_what_was_written() {
        let d = DryRun::new(10 << 20, 512);
        d.write_at(65530, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]).unwrap();
        d.write_at(1 << 20, &[0; 4096]).unwrap();
        let mut b = [0u8; 12];
        d.read_at(65529, &mut b).unwrap();
        assert_eq!(b, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 0]);
        assert_eq!(d.chunks.lock().unwrap().len(), 2, "zero writes to fresh chunks are not stored");
        assert!(d.write_at(10 << 20, &[1]).is_err());
        let small = DryRun { budget: 2 * DRY_CHUNK as usize, ..DryRun::new(10 << 20, 512) };
        assert!(small.write_at(0, &vec![1u8; 3 * DRY_CHUNK as usize]).is_err());
    }
}
