//! Partition table doctor: check, repair (GPT from its backup and vice versa, CRCs, backup
//! relocation, protective MBR), MBR ⇄ GPT conversion in place.
//!
//! Owner: the partitioning workstream.
//!
//! Principles:
//! - Everything is read and analysed first ([`read_disk`] → [`diagnose`]); writes are planned
//!   as a list of [`Write`]s ([`plan_repair`], [`plan_to_gpt`], [`plan_to_mbr`]) and every
//!   one is checked against the partitions before it is applied: the doctor only ever writes
//!   table areas (sector 0, the GPT headers and entry arrays), never partition contents.
//! - Backup structures are written before primary ones, so an interrupted repair leaves at
//!   least one consistent copy; the MBR that makes the new scheme authoritative goes last
//!   when converting to GPT (first when converting to MBR, then the GPT is erased).
//! - Anything ambiguous (two valid but different GPTs, an MBR and a GPT both claiming the
//!   disk, overlapping partitions) is refused with an explanation, never guessed.
//! - On-disk structures are untrusted: every offset and count is checked before use.
//!
//! References: UEFI Specification 2.10 §5.2 (MBR, protective MBR), §5.3 (GPT: header and
//! entry validation, backup placement, CRC32); Microsoft "Windows and GPT FAQ" and the gdisk
//! documentation for the MBR ⇄ GPT type mappings.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use super::job::{JobCtx, Phase};
use super::layout::{self, guid, guids, Apm, GptEntry, GptHeader, MbrEntry, ParsedHeader};
use super::plan::{DoctorAction, DoctorRequest};
use super::target::{self, TargetSpec};
use super::{ForgeError, ForgeResult};
use crate::block::{BlockDev, FileDev, PartitionDev};
use crate::i18n::t;

/// How bad a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// One problem (or remark) about a partition table.
#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    /// Stable identifier for the UI / tests (`gpt_primary_header_crc`, `mbr_overlap`, …).
    pub code: &'static str,
    pub severity: Severity,
    /// Localized explanation.
    pub message: String,
    /// [`DoctorAction::Repair`] fixes it.
    pub repairable: bool,
}

fn finding(code: &'static str, severity: Severity, repairable: bool, it: &str, en: &str) -> Finding {
    Finding { code, severity, message: t(it, en), repairable }
}

/// One copy of a GPT as found on disk.
#[derive(Clone, Debug)]
pub struct GptCopy {
    /// LBA the header was read from.
    pub lba: u64,
    pub ph: ParsedHeader,
    /// The entry array, when the header points to a readable one.
    pub array: Option<Vec<u8>>,
    /// PartitionEntryArrayCRC32 matches.
    pub array_ok: bool,
}

impl GptCopy {
    /// UEFI 2.10 §5.3.2: signature, header CRC, MyLBA = where it was read, entry array CRC.
    pub fn valid(&self) -> bool {
        self.ph.crc_ok && self.ph.sane && self.array_ok && self.ph.h.my_lba == self.lba
    }
    pub fn entries(&self) -> Vec<GptEntry> {
        self.array.as_ref().map(|a| layout::gpt_entries(a, self.ph.h.entry_size)).unwrap_or_default()
    }
}

/// An MBR partition (primary 1-4, logical from 5 in chain order), in sectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MbrPart {
    pub index: u32,
    pub boot: u8,
    pub kind: u8,
    pub start: u64,
    pub count: u64,
}

/// Which partitioning the disk carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskScheme {
    Gpt,
    /// GPT with a hybrid MBR (0xEE next to real MBR partitions).
    Hybrid,
    Mbr,
    Apm,
    None,
}

/// Everything the doctor reads from a disk.
#[derive(Clone, Debug)]
pub struct Disk {
    pub size: u64,
    pub sector: u64,
    pub lbas: u64,
    /// The first 512 bytes.
    pub sector0: Vec<u8>,
    pub mbr_sig: bool,
    /// Sector 0 is a FAT / NTFS / exFAT boot sector (no partition table: "superfloppy").
    pub boot_sector: bool,
    pub mbr: [MbrEntry; 4],
    /// Logical partitions of the extended partition, if any.
    pub logical: Vec<MbrPart>,
    /// What went wrong walking the EBR chain.
    pub ebr_problem: Option<String>,
    pub primary: Option<GptCopy>,
    pub backup: Option<GptCopy>,
    /// A GPT header found at the other sector size (512 vs 4096) than the disk reports.
    pub foreign_sector_gpt: Option<u64>,
    pub apm: Option<Apm>,
}

const EXTENDED: [u8; 3] = [0x05, 0x0F, 0x85];
/// Largest entry array we read (4096 entries × 4096 bytes would be 16 MiB; real ones are 16 KiB).
const MAX_ARRAY: u64 = 4 << 20;

fn is_boot_sector(b: &[u8]) -> bool {
    let oem = &b[3..11];
    if oem == b"NTFS    " || oem == b"EXFAT   " {
        return true;
    }
    let jmp = b[0] == 0xEB || b[0] == 0xE9;
    let bps = u16::from_le_bytes([b[11], b[12]]);
    jmp && matches!(bps, 512 | 1024 | 2048 | 4096) && (&b[54..57] == b"FAT" || &b[82..85] == b"FAT")
}

fn read_gpt_copy(dev: &dyn BlockDev, sector: u64, lbas: u64, lba: u64) -> ForgeResult<Option<GptCopy>> {
    if lba == 0 || lba >= lbas {
        return Ok(None);
    }
    let b = dev.read_vec(lba * sector, sector as usize)?;
    // Structural sanity only (u64::MAX): whether it fits *this* disk is judged separately.
    let Some(ph) = GptHeader::parse(&b, u64::MAX) else { return Ok(None) };
    let h = &ph.h;
    let bytes = h.array_bytes();
    let array = match h.entries_lba.checked_mul(sector) {
        Some(off) if ph.sane && bytes <= MAX_ARRAY && off.checked_add(bytes).is_some_and(|e| e <= dev.size()) => Some(dev.read_vec(off, bytes as usize)?),
        _ => None,
    };
    let array_ok = array.as_ref().is_some_and(|a| layout::crc32(a) == h.entries_crc);
    Ok(Some(GptCopy { lba, ph, array, array_ok }))
}

/// Read sector 0, the EBR chain, both GPT copies and an APM, without judging them.
pub fn read_disk(dev: &dyn BlockDev) -> ForgeResult<Disk> {
    let size = dev.size();
    let sector = layout::sector_size(dev);
    let lbas = size / sector;
    if size < 512 {
        return Err(ForgeError::Invalid(t("il disco è vuoto o troppo piccolo", "the disk is empty or too small")));
    }
    let sector0 = dev.read_vec(0, 512)?;
    let mbr_sig = sector0[510] == 0x55 && sector0[511] == 0xAA;
    let boot_sector = mbr_sig && is_boot_sector(&sector0);
    let mbr = if mbr_sig && !boot_sector { layout::mbr_entries(&sector0) } else { [MbrEntry::default(); 4] };
    let mut d = Disk { size, sector, lbas, sector0, mbr_sig, boot_sector, mbr, logical: vec![], ebr_problem: None, primary: None, backup: None, foreign_sector_gpt: None, apm: None };
    // EBR chain (each EBR: entry 0 = the logical partition relative to this EBR, entry 1 = the
    // next EBR relative to the start of the extended partition).
    if let Some(ext) = d.mbr.iter().find(|e| EXTENDED.contains(&e.kind) && !e.is_empty()) {
        let base = ext.start as u64;
        let mut ebr = base;
        let mut seen = HashSet::new();
        let mut index = 5;
        loop {
            if ebr >= lbas || ebr == 0 {
                d.ebr_problem = Some(format!("EBR at sector {ebr} is outside the disk"));
                break;
            }
            if !seen.insert(ebr) || seen.len() > 128 {
                d.ebr_problem = Some("the EBR chain loops".into());
                break;
            }
            let b = dev.read_vec(ebr * sector, 512)?;
            if b[510] != 0x55 || b[511] != 0xAA {
                d.ebr_problem = Some(format!("EBR at sector {ebr} has no signature"));
                break;
            }
            let e = layout::mbr_entries(&b);
            if !e[0].is_empty() {
                d.logical.push(MbrPart { index, boot: e[0].boot, kind: e[0].kind, start: ebr + e[0].start as u64, count: e[0].count as u64 });
                index += 1;
            }
            if e[1].is_empty() || !EXTENDED.contains(&e[1].kind) {
                break;
            }
            ebr = base + e[1].start as u64;
        }
    }
    d.primary = read_gpt_copy(dev, sector, lbas, 1)?;
    // The backup: where the primary says, else at the last LBA (UEFI 2.10 §5.3.2).
    let alt = d.primary.as_ref().map(|p| p.ph.h.alt_lba).filter(|&a| a > 1 && a < lbas);
    for lba in alt.into_iter().chain(std::iter::once(lbas.saturating_sub(1))) {
        if let Some(c) = read_gpt_copy(dev, sector, lbas, lba)? {
            d.backup = Some(c);
            break;
        }
    }
    if d.primary.is_none() {
        let other = if sector == 512 { 4096 } else { 512 };
        if size >= other * 2 && dev.read_vec(other, 8)? == b"EFI PART" {
            d.foreign_sector_gpt = Some(other);
        }
    }
    if &d.sector0[0..2] == b"ER" {
        d.apm = layout::parse_apm(dev)?;
    }
    Ok(d)
}

impl Disk {
    fn has_ee(&self) -> bool {
        self.mbr.iter().any(|e| e.kind == 0xEE)
    }
    fn mbr_data_entries(&self) -> Vec<MbrPart> {
        self.mbr
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.is_empty() && e.kind != 0xEE && !EXTENDED.contains(&e.kind))
            .map(|(i, e)| MbrPart { index: i as u32 + 1, boot: e.boot, kind: e.kind, start: e.start as u64, count: e.count as u64 })
            .collect()
    }
    /// Primary and logical partitions.
    pub fn mbr_parts(&self) -> Vec<MbrPart> {
        let mut v = self.mbr_data_entries();
        v.extend(self.logical.iter().copied());
        v
    }
    pub fn scheme(&self) -> DiskScheme {
        if self.apm.is_some() {
            return DiskScheme::Apm;
        }
        if self.has_ee() {
            return if self.mbr.iter().any(|e| !e.is_empty() && e.kind != 0xEE) { DiskScheme::Hybrid } else { DiskScheme::Gpt };
        }
        let gpt_found = self.primary.is_some() || self.backup.is_some();
        let mbr_parts = self.mbr.iter().any(|e| !e.is_empty());
        if mbr_parts {
            DiskScheme::Mbr
        } else if gpt_found && !self.boot_sector {
            DiskScheme::Gpt
        } else {
            DiskScheme::None
        }
    }
    /// The GPT copy to believe, if one is valid (primary first).
    pub fn good_gpt(&self) -> Option<&GptCopy> {
        [&self.primary, &self.backup].into_iter().flatten().find(|c| c.valid())
    }
}

fn overlaps(a: (u64, u64), b: (u64, u64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

fn human(bytes: u64) -> String {
    const G: f64 = (1u64 << 30) as f64;
    if bytes >= 1 << 30 {
        format!("{:.2} GiB", bytes as f64 / G)
    } else {
        format!("{:.1} MiB", bytes as f64 / (1u64 << 20) as f64)
    }
}

/// Header fields that must agree between the two GPT copies (the location fields differ by
/// design).
fn same_table(a: &GptCopy, b: &GptCopy) -> bool {
    let (x, y) = (&a.ph.h, &b.ph.h);
    x.disk_guid == y.disk_guid && x.first_usable == y.first_usable && x.last_usable == y.last_usable && x.num_entries == y.num_entries && x.entry_size == y.entry_size && a.array == b.array
}

/// Problems of a set of GPT entries against the header's usable range.
fn gpt_entry_findings(entries: &[GptEntry], h: &GptHeader, out: &mut Vec<Finding>) {
    let mut ranges = vec![];
    let mut guids_seen = HashSet::new();
    for e in entries {
        let n = e.slot + 1;
        if e.last_lba < e.first_lba {
            out.push(finding("gpt_entry_inverted", Severity::Error, false, &format!("la partizione GPT {n} finisce prima di cominciare"), &format!("GPT partition {n} ends before it starts")));
            continue;
        }
        if e.first_lba < h.first_usable || e.last_lba > h.last_usable {
            out.push(finding(
                "gpt_outside_usable",
                Severity::Error,
                false,
                &format!("la partizione GPT {n} (settori {}–{}) esce dall'area utilizzabile ({}–{})", e.first_lba, e.last_lba, h.first_usable, h.last_usable),
                &format!("GPT partition {n} (sectors {}–{}) lies outside the usable area ({}–{})", e.first_lba, e.last_lba, h.first_usable, h.last_usable),
            ));
        }
        for &(m, a, b) in &ranges {
            if overlaps((e.first_lba, e.last_lba.saturating_add(1)), (a, b)) {
                out.push(finding("gpt_overlap", Severity::Error, false, &format!("le partizioni GPT {m} e {n} si sovrappongono"), &format!("GPT partitions {m} and {n} overlap")));
            }
        }
        ranges.push((n, e.first_lba, e.last_lba.saturating_add(1)));
        if !guids_seen.insert(e.unique_guid) {
            out.push(finding("gpt_duplicate_guid", Severity::Warning, false, &format!("la partizione GPT {n} ha lo stesso GUID univoco di un'altra"), &format!("GPT partition {n} has the same unique GUID as another one")));
        }
    }
}

/// Analyse a disk read with [`read_disk`].
pub fn diagnose(d: &Disk) -> Vec<Finding> {
    let mut out = vec![];
    let lbas = d.lbas;
    if let Some(other) = d.foreign_sector_gpt {
        out.push(finding(
            "gpt_sector_size",
            Severity::Error,
            false,
            &format!("la GPT è scritta per settori da {other} byte, ma il disco usa settori da {} byte (immagine copiata da un altro tipo di disco?)", d.sector),
            &format!("the GPT was written for {other}-byte sectors, but the disk uses {}-byte sectors (an image copied from another kind of disk?)", d.sector),
        ));
    }
    match d.scheme() {
        DiskScheme::Apm => {
            let apm = d.apm.as_ref().expect("apm");
            let bs = apm.block_size as u64;
            let mut ranges: Vec<(usize, u64, u64)> = vec![];
            for (i, e) in apm.entries.iter().enumerate() {
                let (s, n) = (e.start as u64, e.count as u64);
                if (s + n) * bs > d.size {
                    out.push(finding("apm_beyond_end", Severity::Error, false, &format!("la voce APM {} ({}) va oltre la fine del disco", i + 1, e.kind), &format!("APM entry {} ({}) goes beyond the end of the disk", i + 1, e.kind)));
                }
                for &(j, a, b) in &ranges {
                    if overlaps((s, s + n), (a, b)) {
                        out.push(finding("apm_overlap", Severity::Error, false, &format!("le voci APM {} e {} si sovrappongono", j + 1, i + 1), &format!("APM entries {} and {} overlap", j + 1, i + 1)));
                    }
                }
                ranges.push((i, s, s + n));
            }
            if apm.entries.is_empty() {
                out.push(finding("apm_empty", Severity::Error, false, "la mappa APM non ha voci leggibili", "the APM map has no readable entries"));
            }
            return out;
        }
        DiskScheme::None => {
            if d.boot_sector {
                out.push(finding("no_table", Severity::Info, false, "nessuna tabella delle partizioni: il filesystem occupa tutto il disco", "no partition table: the filesystem spans the whole disk"));
            } else {
                out.push(finding("no_table", Severity::Info, false, "nessuna tabella delle partizioni", "no partition table"));
            }
            return out;
        }
        DiskScheme::Mbr => {
            mbr_findings(d, &mut out);
            if d.primary.is_some() || d.backup.is_some() {
                out.push(finding(
                    "stale_gpt",
                    Severity::Warning,
                    false,
                    "il disco usa MBR ma contiene anche un'intestazione GPT senza MBR protettivo: alcuni programmi potrebbero leggere la GPT. Se è un residuo, convertire o riformattare la elimina",
                    "the disk uses MBR but also holds a GPT header without a protective MBR: some programs may read the GPT. If it is a leftover, converting or reformatting removes it",
                ));
            }
            return out;
        }
        DiskScheme::Gpt | DiskScheme::Hybrid => {}
    }

    // ── GPT ──
    let copy_findings = |c: &Option<GptCopy>, which: &str, out: &mut Vec<Finding>| {
        let (it_w, en_w) = if which == "primary" { ("primaria", "primary") } else { ("di riserva", "backup") };
        let code = |s: &'static str, b: &'static str| if which == "primary" { s } else { b };
        match c {
            None => out.push(finding(
                code("gpt_primary_missing", "gpt_backup_missing"),
                Severity::Error,
                true,
                &format!("manca l'intestazione GPT {it_w}"),
                &format!("the {en_w} GPT header is missing"),
            )),
            Some(c) => {
                if !c.ph.sane {
                    out.push(finding(code("gpt_primary_header_invalid", "gpt_backup_header_invalid"), Severity::Error, true, &format!("l'intestazione GPT {it_w} ha campi non validi"), &format!("the {en_w} GPT header has invalid fields")));
                } else if !c.ph.crc_ok {
                    out.push(finding(code("gpt_primary_header_crc", "gpt_backup_header_crc"), Severity::Error, true, &format!("il CRC dell'intestazione GPT {it_w} è sbagliato"), &format!("the {en_w} GPT header CRC is wrong")));
                } else if c.ph.h.my_lba != c.lba {
                    out.push(finding(code("gpt_primary_header_invalid", "gpt_backup_header_invalid"), Severity::Error, true, &format!("l'intestazione GPT {it_w} non si trova dove dichiara"), &format!("the {en_w} GPT header is not where it says it is")));
                }
                if c.ph.sane && !c.array_ok {
                    out.push(finding(code("gpt_primary_entries_crc", "gpt_backup_entries_crc"), Severity::Error, true, &format!("la tabella delle voci GPT {it_w} è danneggiata (CRC sbagliato)"), &format!("the {en_w} GPT entry array is damaged (wrong CRC)")));
                }
            }
        }
    };
    copy_findings(&d.primary, "primary", &mut out);
    copy_findings(&d.backup, "backup", &mut out);
    let good = d.good_gpt();
    if let (Some(p), Some(b)) = (&d.primary, &d.backup) {
        if p.valid() && b.valid() && !same_table(p, b) {
            out.push(finding(
                "gpt_mismatch",
                Severity::Error,
                false,
                "la GPT primaria e quella di riserva sono entrambe valide ma diverse: non si può sapere quale sia giusta",
                "the primary and backup GPTs are both valid but different: there is no telling which one is right",
            ));
        }
    }
    if let (Some(p), Some(b)) = (&d.primary, &d.backup) {
        if !p.valid() && !b.valid() {
            if repairable_by_crc(p, b) {
                out.push(finding("gpt_crc_only", Severity::Error, true, "le due copie della GPT coincidono ma hanno CRC sbagliati: si possono ricalcolare", "both GPT copies agree but their CRCs are wrong: they can be recomputed"));
            } else {
                out.push(finding("gpt_no_valid_copy", Severity::Error, false, "nessuna delle due copie della GPT è valida: la tabella non si può ricostruire in modo sicuro", "neither GPT copy is valid: the table can't be rebuilt safely"));
            }
        }
    } else if good.is_none() {
        out.push(finding("gpt_no_valid_copy", Severity::Error, false, "nessuna copia valida della GPT: la tabella non si può ricostruire in modo sicuro", "no valid copy of the GPT: the table can't be rebuilt safely"));
    }
    let reference = good.or_else(|| d.primary.as_ref().filter(|p| d.backup.as_ref().is_some_and(|b| repairable_by_crc(p, b))));
    if let Some(g) = reference {
        let h = &g.ph.h;
        let es = h.array_sectors(d.sector);
        // Where the table thinks the disk ends vs where it ends.
        let table_end = if g.lba == 1 { h.alt_lba } else { g.lba };
        if table_end < lbas - 1 {
            out.push(finding(
                "gpt_backup_not_at_end",
                Severity::Warning,
                true,
                &format!("la GPT di riserva non è alla fine del disco: la tabella è stata creata per un disco di {} e il disco è di {} (immagine copiata su un disco più grande). Lo spazio in più non è utilizzabile finché non si ripara", human(table_end.saturating_add(1).saturating_mul(d.sector)), human(d.size)),
                &format!("the backup GPT is not at the end of the disk: the table was made for a disk of {} and this one is {} (an image written to a larger disk). The extra space can't be used until it is repaired", human(table_end.saturating_add(1).saturating_mul(d.sector)), human(d.size)),
            ));
        } else if table_end > lbas - 1 || h.last_usable >= lbas {
            let entries = g.entries();
            let fits = lbas.checked_sub(2 + es).is_some_and(|last| entries.iter().all(|e| e.last_lba <= last));
            out.push(finding(
                "gpt_beyond_end",
                Severity::Error,
                fits,
                &format!("la GPT descrive un disco di {}, più grande di questo ({}): immagine copiata su un disco più piccolo o troncata{}", human(table_end.saturating_add(1).saturating_mul(d.sector)), human(d.size), if fits { "" } else { "; alcune partizioni non ci stanno" }),
                &format!("the GPT describes a {} disk, larger than this one ({}): an image written to a smaller disk, or truncated{}", human(table_end.saturating_add(1).saturating_mul(d.sector)), human(d.size), if fits { "" } else { "; some partitions don't fit" }),
            ));
        }
        gpt_entry_findings(&g.entries(), h, &mut out);
        let past_end: Vec<u32> = g.entries().iter().filter(|e| e.last_lba >= lbas).map(|e| e.slot + 1).collect();
        if !past_end.is_empty() {
            out.push(finding("gpt_partition_beyond_end", Severity::Error, false, &format!("partizioni GPT oltre la fine del disco: {past_end:?}"), &format!("GPT partitions beyond the end of the disk: {past_end:?}")));
        }
    }

    // ── the MBR of a GPT disk ──
    if !d.mbr_sig || d.boot_sector {
        out.push(finding("pmbr_missing", Severity::Error, true, "manca l'MBR protettivo: sistemi e programmi senza supporto GPT vedrebbero il disco come vuoto", "the protective MBR is missing: systems and programs without GPT support would see an empty disk"));
    } else if !d.has_ee() {
        if d.mbr.iter().any(|e| !e.is_empty()) {
            // Handled as the MBR scheme above; unreachable here.
        } else {
            out.push(finding("pmbr_missing", Severity::Error, true, "il settore 0 non ha la voce protettiva 0xEE della GPT", "sector 0 lacks the GPT's protective 0xEE entry"));
        }
    } else if d.scheme() == DiskScheme::Hybrid {
        out.push(finding(
            "hybrid_mbr",
            Severity::Warning,
            false,
            "MBR ibrido: alcune partizioni GPT sono anche nell'MBR (per l'avvio BIOS). Funziona, ma è fuori standard e va mantenuto coerente a mano",
            "hybrid MBR: some GPT partitions are also in the MBR (for BIOS boot). It works but is non-standard and must be kept consistent by hand",
        ));
        if let Some(g) = reference {
            let gp: Vec<(u64, u64)> = g.entries().iter().map(|e| (e.first_lba, e.last_lba.saturating_sub(e.first_lba).saturating_add(1))).collect();
            for m in d.mbr_data_entries() {
                if !gp.contains(&(m.start, m.count)) {
                    out.push(finding(
                        "hybrid_mismatch",
                        Severity::Error,
                        false,
                        &format!("la voce {} dell'MBR ibrido non corrisponde a nessuna partizione GPT", m.index),
                        &format!("hybrid MBR entry {} matches no GPT partition", m.index),
                    ));
                }
            }
        }
    } else {
        let ee = d.mbr.iter().find(|e| e.kind == 0xEE).copied().unwrap_or_default();
        let want = layout::protective_entry(lbas);
        // UEFI 2.10 Table 5.4: StartingLBA 1, SizeInLBA = disk size − 1 (or 0xFFFFFFFF).
        if ee.start != 1 || (ee.count != want.count && ee.count != u32::MAX) {
            out.push(finding(
                "pmbr_size",
                Severity::Warning,
                true,
                &format!("la voce protettiva dell'MBR copre {} invece dell'intero disco ({})", human((ee.count as u64 + 1) * d.sector), human(d.size)),
                &format!("the protective MBR entry covers {} instead of the whole disk ({})", human((ee.count as u64 + 1) * d.sector), human(d.size)),
            ));
        }
    }
    out
}

fn mbr_findings(d: &Disk, out: &mut Vec<Finding>) {
    let parts = d.mbr_parts();
    let lbas = d.lbas;
    let mut seen: Vec<&MbrPart> = vec![];
    let actives = d.mbr.iter().filter(|e| !e.is_empty() && e.boot == 0x80).count();
    for p in &parts {
        let n = p.index;
        if p.start == 0 {
            out.push(finding("mbr_starts_at_zero", Severity::Error, false, &format!("la partizione MBR {n} comincia nel settore 0, sopra l'MBR stesso"), &format!("MBR partition {n} starts at sector 0, on top of the MBR itself")));
        }
        if p.start + p.count > lbas {
            out.push(finding(
                "mbr_beyond_end",
                Severity::Error,
                false,
                &format!("la partizione MBR {n} va oltre la fine del disco ({} oltre)", human((p.start + p.count - lbas) * d.sector)),
                &format!("MBR partition {n} goes beyond the end of the disk (by {})", human((p.start + p.count - lbas) * d.sector)),
            ));
        }
        if p.boot != 0 && p.boot != 0x80 {
            out.push(finding("mbr_bad_boot_flag", Severity::Warning, false, &format!("la partizione MBR {n} ha un indicatore di avvio non valido (0x{:02X})", p.boot), &format!("MBR partition {n} has an invalid boot indicator (0x{:02X})", p.boot)));
        }
        for q in &seen {
            if overlaps((p.start, p.start + p.count), (q.start, q.start + q.count)) {
                out.push(finding("mbr_overlap", Severity::Error, false, &format!("le partizioni MBR {} e {n} si sovrappongono", q.index), &format!("MBR partitions {} and {n} overlap", q.index)));
            }
        }
        seen.push(p);
    }
    // The extended container must hold its logical partitions.
    if let Some(ext) = d.mbr.iter().find(|e| EXTENDED.contains(&e.kind) && !e.is_empty()) {
        let (a, b) = (ext.start as u64, ext.start as u64 + ext.count as u64);
        if b > lbas {
            out.push(finding("mbr_beyond_end", Severity::Error, false, "la partizione estesa va oltre la fine del disco", "the extended partition goes beyond the end of the disk"));
        }
        for l in &d.logical {
            if l.start < a || l.start + l.count > b {
                out.push(finding("mbr_logical_outside", Severity::Error, false, &format!("la partizione logica {} esce dalla partizione estesa", l.index), &format!("logical partition {} lies outside the extended partition", l.index)));
            }
        }
        for p in d.mbr_data_entries() {
            if overlaps((p.start, p.start + p.count), (a, b)) {
                out.push(finding("mbr_overlap", Severity::Error, false, &format!("la partizione MBR {} si sovrappone alla partizione estesa", p.index), &format!("MBR partition {} overlaps the extended partition", p.index)));
            }
        }
    }
    if let Some(why) = &d.ebr_problem {
        out.push(finding("ebr_chain", Severity::Error, false, &format!("catena delle partizioni logiche danneggiata: {why}"), &format!("damaged chain of logical partitions: {why}")));
    }
    if actives > 1 {
        out.push(finding("mbr_multiple_active", Severity::Warning, false, "più di una partizione MBR è marcata attiva", "more than one MBR partition is marked active"));
    }
}

/// Both copies structurally fine, only their CRCs wrong, and saying the same thing.
fn repairable_by_crc(p: &GptCopy, b: &GptCopy) -> bool {
    p.ph.sane && b.ph.sane && p.array.is_some() && p.array == b.array && p.ph.h.my_lba == p.lba && b.ph.h.my_lba == b.lba && same_table(p, b)
}

// ───────────────────────────── planned writes ─────────────────────────────

/// One planned write to a table area.
#[derive(Clone, Debug)]
pub struct Write {
    pub off: u64,
    pub data: Vec<u8>,
    /// Localized description for the log / result.
    pub what: String,
}

/// Refuse any write that would touch a partition (`parts`: byte ranges [start, end)).
fn check_writes(writes: &[Write], parts: &[(u64, u64)], size: u64) -> ForgeResult<()> {
    for w in writes {
        let end = w.off.checked_add(w.data.len() as u64).ok_or_else(|| ForgeError::Unsafe("write range overflow".into()))?;
        if end > size || parts.iter().any(|&p| overlaps((w.off, end), p)) {
            return Err(ForgeError::Unsafe(t(
                &format!("rifiutato: la scrittura \"{}\" toccherebbe i dati di una partizione", w.what),
                &format!("refused: writing \"{}\" would touch a partition's data", w.what),
            )));
        }
    }
    Ok(())
}

fn refuse(it: &str, en: &str) -> ForgeError {
    ForgeError::Invalid(t(it, en))
}

fn unrepairable_errors(f: &[Finding]) -> Vec<String> {
    f.iter().filter(|f| f.severity == Severity::Error && !f.repairable).map(|f| f.message.clone()).collect()
}

/// Byte ranges of GPT partitions.
fn gpt_ranges(entries: &[GptEntry], sector: u64) -> Vec<(u64, u64)> {
    entries.iter().map(|e| (e.first_lba.saturating_mul(sector), e.last_lba.saturating_add(1).saturating_mul(sector))).collect()
}

/// A new sector 0 for a GPT disk: protective entry, boot code and disk signature kept.
fn protective_sector0(d: &Disk, disk_guid: &[u8; 16]) -> Vec<u8> {
    let keep = d.mbr_sig && !d.boot_sector;
    let code = if keep { d.sector0[..440].to_vec() } else { vec![] };
    let sig = if keep && d.sector0[440..444] != [0; 4] { [d.sector0[440], d.sector0[441], d.sector0[442], d.sector0[443]] } else { [disk_guid[0], disk_guid[1], disk_guid[2], disk_guid[3]] };
    let boot = d.mbr.iter().find(|e| e.kind == 0xEE).map(|e| e.boot).unwrap_or(0);
    let mut ee = layout::protective_entry(d.lbas);
    ee.boot = boot;
    layout::build_mbr(&code, sig, &[ee])
}

/// What [`DoctorAction::Repair`] would write, in order. Ok(empty) when there is nothing to
/// repair; Err when the disk has problems that can't be repaired safely.
pub fn plan_repair(d: &Disk) -> ForgeResult<Vec<Write>> {
    let findings = diagnose(d);
    let bad = unrepairable_errors(&findings);
    if !bad.is_empty() {
        return Err(refuse(&format!("non riparabile automaticamente: {}", bad.join("; ")), &format!("can't be repaired automatically: {}", bad.join("; "))));
    }
    if !findings.iter().any(|f| f.repairable) {
        return Ok(vec![]);
    }
    if !matches!(d.scheme(), DiskScheme::Gpt | DiskScheme::Hybrid) {
        return Ok(vec![]);
    }
    let sector = d.sector;
    let lbas = d.lbas;
    // The copy to rebuild from.
    let src: GptCopy = match (d.primary.as_ref().filter(|c| c.valid()), d.backup.as_ref().filter(|c| c.valid())) {
        (Some(p), _) => p.clone(),
        (None, Some(b)) => b.clone(),
        (None, None) => match (&d.primary, &d.backup) {
            (Some(p), Some(b)) if repairable_by_crc(p, b) => p.clone(),
            _ => return Err(refuse("nessuna copia valida della GPT da cui ripartire", "no valid GPT copy to rebuild from")),
        },
    };
    let array = src.array.clone().ok_or_else(|| refuse("tabella delle voci GPT illeggibile", "unreadable GPT entry array"))?;
    let h = src.ph.h.clone();
    let es = h.array_sectors(sector);
    let entries = layout::gpt_entries(&array, h.entry_size);
    let backup_arr = lbas.checked_sub(1 + es).ok_or_else(|| refuse("disco troppo piccolo per la GPT", "disk too small for the GPT"))?;
    let new_last = backup_arr.checked_sub(1).ok_or_else(|| refuse("disco troppo piccolo per la GPT", "disk too small for the GPT"))?;
    // Where the table thinks the disk ends: relocate the backup to the real end when it differs,
    // and make the usable area follow (grow onto a larger disk, shrink onto a smaller one when
    // every partition still fits).
    let table_end = if src.lba == 1 { h.alt_lba } else { src.lba };
    let last_usable = if table_end != lbas - 1 || h.last_usable > new_last { new_last } else { h.last_usable };
    if let Some(e) = entries.iter().find(|e| e.last_lba > last_usable || e.first_lba < h.first_usable || e.last_lba < e.first_lba) {
        return Err(refuse(
            &format!("la partizione GPT {} non sta nell'area utilizzabile ricostruita: riparazione rifiutata", e.slot + 1),
            &format!("GPT partition {} doesn't fit the rebuilt usable area: repair refused", e.slot + 1),
        ));
    }
    // The primary array where a valid primary has it, else at LBA 2 (UEFI 2.10 §5.3.2).
    let prim_arr = if src.lba == 1 { h.entries_lba } else { 2 };
    if prim_arr < 2 || prim_arr.checked_add(es).map_or(true, |e| e > h.first_usable) {
        return Err(refuse("la GPT primaria non ha spazio prima della prima partizione", "there is no room for the primary GPT before the first partition"));
    }
    let primary = GptHeader { header_size: 92, my_lba: 1, alt_lba: lbas - 1, last_usable, entries_lba: prim_arr, entries_crc: layout::crc32(&array), ..h.clone() };
    let backup = primary.mirrored(backup_arr);
    let mut writes = vec![];
    let backup_ok = d.backup.as_ref().is_some_and(|b| b.valid() && b.lba == lbas - 1 && b.ph.h == backup && b.array.as_deref() == Some(&array[..]));
    let primary_ok = d.primary.as_ref().is_some_and(|p| p.valid() && p.ph.h == primary && p.array.as_deref() == Some(&array[..]));
    if !backup_ok {
        writes.push(Write { off: backup_arr * sector, data: array.clone(), what: t("voci GPT di riserva", "backup GPT entries") });
        writes.push(Write { off: (lbas - 1) * sector, data: backup.to_bytes(sector), what: t("intestazione GPT di riserva", "backup GPT header") });
    }
    if !primary_ok {
        writes.push(Write { off: prim_arr * sector, data: array.clone(), what: t("voci GPT primarie", "primary GPT entries") });
        writes.push(Write { off: sector, data: primary.to_bytes(sector), what: t("intestazione GPT primaria", "primary GPT header") });
    }
    // An old backup left behind at the previous end of the disk: erase its header so no tool
    // mistakes it for the real one (it is in free space now).
    if let Some(b) = &d.backup {
        if b.lba != lbas - 1 {
            writes.push(Write { off: b.lba * sector, data: vec![0u8; sector as usize], what: t("vecchia intestazione GPT di riserva", "old backup GPT header") });
        }
    }
    let pmbr_broken = findings.iter().any(|f| matches!(f.code, "pmbr_missing" | "pmbr_size"));
    if pmbr_broken && d.scheme() != DiskScheme::Hybrid {
        writes.push(Write { off: 0, data: protective_sector0(d, &h.disk_guid), what: t("MBR protettivo", "protective MBR") });
    }
    check_writes(&writes, &gpt_ranges(&entries, sector), d.size)?;
    Ok(writes)
}

/// MBR type → GPT type GUID (gdisk's and Microsoft's correspondences).
pub fn mbr_to_gpt_type(t: u8) -> Option<(&'static str, &'static str)> {
    Some(match t {
        0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E | 0x11 | 0x14 | 0x16 | 0x17 | 0x1B | 0x1C | 0x1E => (guids::MS_BASIC_DATA, "Microsoft basic data"),
        0x27 => (guids::MS_RECOVERY, "Windows recovery"),
        0x82 => (guids::LINUX_SWAP, "Linux swap"),
        0x83 => (guids::LINUX_FS, "Linux filesystem"),
        0x8E => (guids::LINUX_LVM, "Linux LVM"),
        0xFD => (guids::LINUX_RAID, "Linux RAID"),
        0xEF => (guids::EFI_SYSTEM, "EFI system partition"),
        0xAF => (guids::APPLE_HFS, "Apple HFS+"),
        0xA5 => (guids::FREEBSD_DATA, "FreeBSD"),
        _ => return None,
    })
}

/// What [`DoctorAction::ConvertToGpt`] would write (and warnings for the result).
pub fn plan_to_gpt(d: &Disk) -> ForgeResult<(Vec<Write>, Vec<String>, Vec<GptEntry>)> {
    match d.scheme() {
        DiskScheme::Mbr => {}
        DiskScheme::Gpt | DiskScheme::Hybrid => return Err(refuse("il disco usa già GPT", "the disk already uses GPT")),
        DiskScheme::Apm => return Err(ForgeError::Unsupported(t("la conversione da APM non è supportata", "converting from APM is not supported"))),
        DiskScheme::None => return Err(refuse("il disco non ha una tabella MBR da convertire", "the disk has no MBR partition table to convert")),
    }
    let bad = unrepairable_errors(&diagnose(d));
    if !bad.is_empty() {
        return Err(refuse(&format!("prima vanno risolti questi problemi: {}", bad.join("; ")), &format!("these problems must be solved first: {}", bad.join("; "))));
    }
    let sector = d.sector;
    let lbas = d.lbas;
    let mut parts = d.mbr_parts();
    parts.sort_by_key(|p| p.start);
    if parts.is_empty() {
        return Err(refuse("nessuna partizione da convertire", "no partition to convert"));
    }
    if parts.len() > layout::GPT_ENTRIES as usize {
        return Err(refuse("troppe partizioni per la GPT", "too many partitions for GPT"));
    }
    if d.mbr.iter().any(|e| e.kind == 0x42) {
        return Err(refuse(
            "è un disco dinamico di Windows (LDM): va convertito in base da Windows prima",
            "it is a Windows dynamic disk (LDM): convert it to basic in Windows first",
        ));
    }
    let (first_usable, last_usable, backup_arr) = layout::gpt_geometry(lbas, sector).ok_or_else(|| refuse("disco troppo piccolo per la GPT", "disk too small for GPT"))?;
    let es = layout::GPT_ARRAY_BYTES.div_ceil(sector);
    let first = parts[0];
    if first.start < first_usable {
        return Err(refuse(
            &format!("la prima partizione comincia al settore {}: la GPT ha bisogno dei primi {first_usable} settori liberi ({} byte). Sposta o restringi la partizione con un programma di partizionamento e riprova", first.start, first_usable * sector),
            &format!("the first partition starts at sector {}: GPT needs the first {first_usable} sectors free ({} bytes). Move or shrink the partition with a partitioning tool and try again", first.start, first_usable * sector),
        ));
    }
    let end = parts.iter().map(|p| p.start + p.count).max().unwrap_or(0);
    if end > last_usable + 1 {
        let need = 1 + es;
        return Err(refuse(
            &format!("l'ultima partizione arriva a {} settori dalla fine del disco: la GPT di riserva ha bisogno degli ultimi {need} settori ({} byte). Restringi l'ultima partizione di almeno {} KiB e riprova", lbas - end, need * sector, ((end - last_usable - 1) * sector).div_ceil(1024)),
            &format!("the last partition ends {} sectors before the end of the disk: the backup GPT needs the last {need} sectors ({} bytes). Shrink the last partition by at least {} KiB and try again", lbas - end, need * sector, ((end - last_usable - 1) * sector).div_ceil(1024)),
        ));
    }
    let mut entries = vec![];
    for (slot, p) in parts.iter().enumerate() {
        let Some((g, name)) = mbr_to_gpt_type(p.kind) else {
            return Err(refuse(
                &format!("la partizione {} ha il tipo MBR 0x{:02X}, per cui LumenDisk non conosce un tipo GPT equivalente", p.index, p.kind),
                &format!("partition {} has MBR type 0x{:02X}, for which LumenDisk knows no GPT equivalent", p.index, p.kind),
            ));
        };
        entries.push(GptEntry {
            slot: slot as u32,
            type_guid: guid(g),
            unique_guid: layout::random16(),
            first_lba: p.start,
            last_lba: p.start + p.count - 1,
            // UEFI 2.10 Table 5.8 bit 2: legacy BIOS bootable (keeps the active flag).
            attrs: if p.boot == 0x80 { 1 << 2 } else { 0 },
            name: name.into(),
        });
    }
    let mut warnings = vec![];
    if d.sector0[..440].iter().any(|&b| b != 0) || parts.iter().any(|p| p.boot == 0x80) {
        warnings.push(t(
            "il codice di avvio BIOS è stato mantenuto, ma un disco GPT si avvia in modalità UEFI: Windows su MBR andrà reinstallato o convertito con mbr2gpt per avviarsi",
            "the BIOS boot code was kept, but a GPT disk boots in UEFI mode: Windows installed on MBR needs mbr2gpt (or a reinstall) to boot",
        ));
    }
    let array = layout::gpt_array(&entries)?;
    let disk_guid = layout::random16();
    let primary = GptHeader {
        revision: layout::GPT_REVISION,
        header_size: 92,
        my_lba: 1,
        alt_lba: lbas - 1,
        first_usable,
        last_usable,
        disk_guid,
        entries_lba: 2,
        num_entries: layout::GPT_ENTRIES,
        entry_size: layout::GPT_ENTRY_SIZE,
        entries_crc: layout::crc32(&array),
    };
    let backup = primary.mirrored(backup_arr);
    let mut writes = vec![
        Write { off: backup_arr * sector, data: array.clone(), what: t("voci GPT di riserva", "backup GPT entries") },
        Write { off: (lbas - 1) * sector, data: backup.to_bytes(sector), what: t("intestazione GPT di riserva", "backup GPT header") },
        Write { off: 2 * sector, data: array, what: t("voci GPT primarie", "primary GPT entries") },
        Write { off: sector, data: primary.to_bytes(sector), what: t("intestazione GPT primaria", "primary GPT header") },
    ];
    // Protective MBR last: until it is written the disk stays a valid MBR disk.
    let keep_sig = [d.sector0[440], d.sector0[441], d.sector0[442], d.sector0[443]];
    let sig = if keep_sig != [0; 4] { keep_sig } else { [disk_guid[0], disk_guid[1], disk_guid[2], disk_guid[3]] };
    writes.push(Write { off: 0, data: layout::build_mbr(&d.sector0[..440], sig, &[layout::protective_entry(lbas)]), what: t("MBR protettivo", "protective MBR") });
    let ranges: Vec<(u64, u64)> = parts.iter().map(|p| (p.start * sector, (p.start + p.count) * sector)).collect();
    check_writes(&writes, &ranges, d.size)?;
    Ok((writes, warnings, entries))
}

/// GPT type → MBR type byte; `probe`: the filesystem found in the partition (for Microsoft
/// basic data, which covers FAT, exFAT and NTFS alike).
pub fn gpt_to_mbr_type(type_guid: &[u8; 16], probe: Option<crate::vfs::FsType>) -> Result<u8, &'static str> {
    use crate::vfs::FsType;
    let g = layout::guid_string(type_guid);
    Ok(match g.as_str() {
        guids::MS_BASIC_DATA => match probe {
            Some(FsType::Fat12) => 0x01,
            Some(FsType::Fat16) => 0x0E,
            Some(FsType::Fat32) => 0x0C,
            Some(FsType::Ext2 | FsType::Ext3 | FsType::Ext4) => 0x83,
            _ => 0x07,
        },
        guids::EFI_SYSTEM => 0xEF,
        guids::MS_RECOVERY => 0x27,
        guids::LINUX_FS | "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709" | "B921B045-1DF0-41C3-AF44-4C6F280D3FAE" => 0x83,
        guids::LINUX_SWAP => 0x82,
        guids::LINUX_LVM => 0x8E,
        guids::LINUX_RAID => 0xFD,
        guids::APPLE_HFS => 0xAF,
        guids::FREEBSD_DATA | guids::FREEBSD_UFS => 0xA5,
        guids::MS_RESERVED => return Err("Microsoft reserved (MSR)"),
        guids::BIOS_BOOT => return Err("BIOS boot"),
        guids::MS_LDM_DATA => return Err("Windows LDM (dynamic disk)"),
        guids::APPLE_APFS => return Err("APFS"),
        _ => return Err("unknown"),
    })
}

/// What [`DoctorAction::ConvertToMbr`] would write (MBR first, then the GPT erased: backup
/// header and array, primary array and header).
pub fn plan_to_mbr(d: &Disk, dev: &Arc<dyn BlockDev>) -> ForgeResult<(Vec<Write>, Vec<String>, Vec<MbrEntry>)> {
    match d.scheme() {
        DiskScheme::Gpt | DiskScheme::Hybrid => {}
        DiskScheme::Mbr => return Err(refuse("il disco usa già MBR", "the disk already uses MBR")),
        DiskScheme::Apm => return Err(ForgeError::Unsupported(t("la conversione da APM non è supportata", "converting from APM is not supported"))),
        DiskScheme::None => return Err(refuse("il disco non ha una GPT da convertire", "the disk has no GPT to convert")),
    }
    let findings = diagnose(d);
    if findings.iter().any(|f| f.severity == Severity::Error) {
        return Err(refuse(
            "la GPT ha dei problemi: esegui prima \"Ripara\" (o controlla il disco) e poi riprova la conversione",
            "the GPT has problems: run \"Repair\" first (or check the disk), then convert",
        ));
    }
    let g = d.good_gpt().ok_or_else(|| refuse("nessuna GPT valida", "no valid GPT"))?;
    let sector = d.sector;
    let mut entries = g.entries();
    entries.sort_by_key(|e| e.first_lba);
    if entries.len() > 4 {
        return Err(refuse(
            &format!("ci sono {} partizioni: l'MBR ne può avere al massimo 4 (le partizioni logiche non sono supportate dalla conversione)", entries.len()),
            &format!("there are {} partitions: MBR holds at most 4 (logical partitions are not supported by the conversion)", entries.len()),
        ));
    }
    let mut mbr = vec![];
    let mut warnings = vec![];
    for e in &entries {
        let n = e.slot + 1;
        let count = e.last_lba.saturating_sub(e.first_lba).saturating_add(1);
        if e.first_lba > u32::MAX as u64 || count > u32::MAX as u64 || e.last_lba >= 1u64 << 32 {
            return Err(refuse(
                &format!("la partizione {n} va oltre i 2 TiB indirizzabili dall'MBR (con settori da {sector} byte)"),
                &format!("partition {n} goes beyond the 2 TiB MBR can address (with {sector}-byte sectors)"),
            ));
        }
        let probe = {
            let view: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), e.first_lba * sector, count * sector)?);
            crate::vfs::probe(view.as_ref())
        };
        let kind = gpt_to_mbr_type(&e.type_guid, probe).map_err(|what| {
            refuse(
                &format!("la partizione {n} è di tipo {what} ({}), che non ha un equivalente MBR: rimuovila prima, se non serve", layout::guid_string(&e.type_guid)),
                &format!("partition {n} is of type {what} ({}), which has no MBR equivalent: remove it first if it is not needed", layout::guid_string(&e.type_guid)),
            )
        })?;
        mbr.push(MbrEntry { boot: if e.attrs & (1 << 2) != 0 { 0x80 } else { 0 }, kind, start: e.first_lba as u32, count: count as u32 });
    }
    if !entries.is_empty() && d.scheme() == DiskScheme::Hybrid {
        warnings.push(t("l'MBR ibrido è stato sostituito da un MBR normale con tutte le partizioni", "the hybrid MBR was replaced by a plain MBR with every partition"));
    }
    let h = &g.ph.h;
    let es = h.array_sectors(sector);
    let lbas = d.lbas;
    let code = if d.mbr_sig && !d.boot_sector { d.sector0[..440].to_vec() } else { vec![] };
    let keep_sig = [d.sector0[440], d.sector0[441], d.sector0[442], d.sector0[443]];
    let sig = if d.mbr_sig && keep_sig != [0; 4] { keep_sig } else { [h.disk_guid[0], h.disk_guid[1], h.disk_guid[2], h.disk_guid[3]] };
    let mut writes = vec![Write { off: 0, data: layout::build_mbr(&code, sig, &mbr), what: "MBR".into() }];
    let zero = |lba: u64, sectors: u64, what: String| Write { off: lba * sector, data: vec![0u8; (sectors * sector) as usize], what };
    for c in [&d.backup, &d.primary].into_iter().flatten() {
        if c.lba < lbas {
            if let Some(arr) = c.ph.h.entries_lba.checked_add(es).filter(|&e| e <= lbas).map(|_| c.ph.h.entries_lba) {
                writes.push(zero(arr, es, t("vecchie voci GPT", "old GPT entries")));
            }
            writes.push(zero(c.lba, 1, t("vecchia intestazione GPT", "old GPT header")));
        }
    }
    let ranges = gpt_ranges(&entries, sector);
    check_writes(&writes, &ranges, d.size)?;
    Ok((writes, warnings, mbr))
}

// ───────────────────────────── running ─────────────────────────────

fn partitions_json(d: &Disk) -> Value {
    let s = d.sector;
    match d.scheme() {
        DiskScheme::Gpt | DiskScheme::Hybrid => {
            let g = d.good_gpt().or(d.primary.as_ref()).or(d.backup.as_ref());
            let v: Vec<Value> = g
                .map(|g| g.entries())
                .unwrap_or_default()
                .iter()
                .map(|e| {
                    json!({"index": e.slot + 1, "start": e.first_lba.saturating_mul(s), "size": e.last_lba.saturating_sub(e.first_lba).saturating_add(1).saturating_mul(s),
                           "type": layout::guid_string(&e.type_guid), "name": e.name})
                })
                .collect();
            Value::Array(v)
        }
        DiskScheme::Mbr => Value::Array(d.mbr_parts().iter().map(|p| json!({"index": p.index, "start": p.start * s, "size": p.count * s, "type": format!("0x{:02X}", p.kind), "name": "", "active": p.boot == 0x80})).collect()),
        DiskScheme::Apm => {
            let apm = d.apm.as_ref().expect("apm");
            let bs = apm.block_size as u64;
            Value::Array(apm.entries.iter().enumerate().map(|(i, e)| json!({"index": i + 1, "start": e.start as u64 * bs, "size": e.count as u64 * bs, "type": e.kind, "name": e.name})).collect())
        }
        DiskScheme::None => Value::Array(vec![]),
    }
}

/// The JSON report of a disk.
pub fn report(d: &Disk) -> Value {
    let findings = diagnose(d);
    let healthy = !findings.iter().any(|f| f.severity >= Severity::Warning);
    json!({
        "scheme": d.scheme(),
        "sector_size": d.sector,
        "disk_size": d.size,
        "partitions": partitions_json(d),
        "findings": findings,
        "healthy": healthy,
        "repairable": findings.iter().any(|f| f.repairable) && unrepairable_errors(&findings).is_empty(),
    })
}

fn apply(dev: &dyn BlockDev, writes: &[Write], ctx: &JobCtx) -> ForgeResult<Vec<String>> {
    ctx.phase_units(Phase::Writing, writes.len() as u64, "steps");
    let mut done = vec![];
    for w in writes {
        ctx.info(format!("{} @ {}", w.what, w.off));
        dev.write_at(w.off, &w.data)?;
        // Durable in order: a later write never lands before an earlier one.
        dev.flush()?;
        done.push(w.what.clone());
        ctx.set_done(done.len() as u64);
    }
    Ok(done)
}

/// Run `action` on an open device (the body of [`run`]; tests call it on RAM disks).
pub fn doctor_device(dev: Arc<dyn BlockDev>, action: DoctorAction, ctx: &JobCtx) -> ForgeResult<Value> {
    ctx.phase(Phase::Verifying, 0);
    let d = read_disk(dev.as_ref())?;
    let before = report(&d);
    if action == DoctorAction::Check {
        return Ok(before);
    }
    ctx.check()?;
    let (writes, mut warnings) = match action {
        DoctorAction::Check => unreachable!(),
        DoctorAction::Repair => (plan_repair(&d)?, vec![]),
        DoctorAction::ConvertToGpt => {
            let (w, warn, _) = plan_to_gpt(&d)?;
            (w, warn)
        }
        DoctorAction::ConvertToMbr => {
            let (w, warn, _) = plan_to_mbr(&d, &dev)?;
            (w, warn)
        }
    };
    if action == DoctorAction::ConvertToGpt {
        // Non-zero bytes about to be overwritten in the new table areas (e.g. a BIOS boot
        // loader embedded after the MBR) deserve a warning; they are not partition data.
        for w in writes.iter().filter(|w| w.off != 0) {
            let old = dev.read_vec(w.off, w.data.len())?;
            if old.iter().any(|&b| b != 0) {
                warnings.push(t(
                    &format!("l'area della nuova {} (byte {}) non era vuota: probabilmente un boot loader BIOS (p.es. GRUB) nello spazio dopo l'MBR, che andrà reinstallato", w.what, w.off),
                    &format!("the area of the new {} (byte {}) was not empty: probably a BIOS boot loader (e.g. GRUB) after the MBR, which will need reinstalling", w.what, w.off),
                ));
            }
        }
    }
    for w in &warnings {
        ctx.warn(w.clone());
    }
    ctx.check()?;
    let actions = apply(dev.as_ref(), &writes, ctx)?;
    ctx.phase(Phase::Verifying, 0);
    let after_disk = read_disk(dev.as_ref())?;
    let after = report(&after_disk);
    let remaining = diagnose(&after_disk);
    let errors: Vec<String> = remaining.iter().filter(|f| f.severity == Severity::Error).map(|f| f.message.clone()).collect();
    if !errors.is_empty() {
        return Err(ForgeError::Mismatch(t(&format!("dopo la scrittura restano problemi: {}", errors.join("; ")), &format!("problems remain after writing: {}", errors.join("; ")))));
    }
    if actions.is_empty() {
        ctx.info(t("niente da riparare", "nothing to repair"));
    }
    Ok(json!({
        "action": action,
        "actions": actions,
        "warnings": warnings,
        "before": before,
        "after": after,
    }))
}

/// Open a doctor target: a disk read-only for checks (no unmount needed) or through the safety
/// policy for writing; an existing image file (never created, truncated or resized here).
fn open(spec: &TargetSpec, write: bool, ctx: &JobCtx) -> ForgeResult<(Arc<dyn BlockDev>, Box<dyn std::any::Any>)> {
    match spec {
        TargetSpec::Disk { bsd } if write => {
            let t = target::open_for_write(spec, ctx)?;
            let _ = bsd;
            Ok((t.dev.clone(), Box::new(t)))
        }
        TargetSpec::Disk { bsd } => {
            let info = target::disk_info(bsd)?;
            if !info.whole {
                return Err(ForgeError::Invalid(t("scegli un disco intero, non una partizione", "choose a whole disk, not a partition")));
            }
            let dev = crate::block::open_source(&format!("/dev/r{}", info.bsd), true, None)?;
            Ok((dev, Box::new(())))
        }
        TargetSpec::File { path, .. } => {
            if !path.is_file() {
                return Err(ForgeError::Invalid(t(&format!("{} non esiste", path.display()), &format!("{} does not exist", path.display()))));
            }
            let claim = if write { Some(target::claim(&path.to_string_lossy())?) } else { None };
            let dev: Arc<dyn BlockDev> = Arc::new(FileDev::open(path, !write)?);
            Ok((dev, Box::new(claim)))
        }
    }
}

pub fn run(r: &DoctorRequest, ctx: &JobCtx) -> ForgeResult<Value> {
    ctx.phase(Phase::Preparing, 0);
    let (dev, _hold) = open(&r.target, r.action != DoctorAction::Check, ctx)?;
    doctor_device(dev, r.action, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{parse_partitions, MemDev};
    use crate::forge::layout::tests::{Rng, SectorDev};
    use crate::forge::layout::{NewPart, PartType, TableOptions};
    use crate::forge::plan::Scheme;
    use crate::forge::TargetFs;

    fn codes(d: &dyn BlockDev) -> Vec<&'static str> {
        diagnose(&read_disk(d).unwrap()).iter().map(|f| f.code).collect()
    }

    fn ctx() -> JobCtx {
        JobCtx::detached(false)
    }

    fn gpt_disk(mb: usize, sector: u32, parts: &[(u64, TargetFs)]) -> Arc<SectorDev> {
        let dev = Arc::new(SectorDev::new(mb << 20, sector));
        let np: Vec<NewPart> = parts
            .iter()
            .enumerate()
            .map(|(i, &(sz, fs))| NewPart { size: if sz == 0 { None } else { Some(sz) }, kind: PartType::Fs(fs), name: format!("part{i}"), bootable: i == 0 })
            .collect();
        layout::write_table(dev.as_ref(), Scheme::Gpt, &np, &TableOptions::default()).unwrap();
        dev
    }

    fn entries_of(dev: &dyn BlockDev) -> Vec<GptEntry> {
        read_disk(dev).unwrap().good_gpt().unwrap().entries()
    }

    fn run_on(dev: Arc<dyn BlockDev>, a: DoctorAction) -> ForgeResult<Value> {
        doctor_device(dev, a, &ctx())
    }

    #[test]
    fn a_fresh_gpt_is_healthy_and_repair_writes_nothing() {
        for sector in [512, 4096] {
            let dev = gpt_disk(64, sector, &[(8 << 20, TargetFs::Fat32), (0, TargetFs::Ext4)]);
            let r = run_on(dev.clone(), DoctorAction::Check).unwrap();
            assert_eq!(r["healthy"], true, "{r}");
            assert_eq!(r["scheme"], "gpt");
            assert_eq!(r["partitions"].as_array().unwrap().len(), 2);
            let before = dev.mem.snapshot();
            let r = run_on(dev.clone(), DoctorAction::Repair).unwrap();
            assert!(r["actions"].as_array().unwrap().is_empty());
            assert!(dev.mem.snapshot() == before);
        }
    }

    /// Corrupt one structure, check the finding, repair, compare with the original table.
    #[test]
    fn single_corruptions_are_found_and_repaired() {
        type Corrupt = fn(&SectorDev, u64, u64);
        let cases: Vec<(&str, Corrupt)> = vec![
            ("gpt_primary_header_crc", |d, s, _| d.write_at(s + 40, &[0x99]).unwrap()),
            ("gpt_primary_missing", |d, s, _| d.write_at(s, &vec![0u8; s as usize]).unwrap()),
            ("gpt_primary_entries_crc", |d, s, _| d.write_at(2 * s + 20, &[0x77]).unwrap()),
            ("gpt_backup_header_crc", |d, s, l| d.write_at((l - 1) * s + 48, &[0x55]).unwrap()),
            ("gpt_backup_missing", |d, s, l| d.write_at((l - 1) * s, &vec![0u8; s as usize]).unwrap()),
            ("gpt_backup_entries_crc", |d, s, l| d.write_at((l - 1 - 16384 / s) * s + 3, &[0x11]).unwrap()),
            ("pmbr_missing", |d, _, _| d.write_at(0, &[0u8; 512]).unwrap()),
            ("pmbr_size", |d, _, _| d.write_at(446 + 12, &1000u32.to_le_bytes()).unwrap()),
        ];
        for sector in [512u64, 4096] {
            for (code, corrupt) in &cases {
                let dev = gpt_disk(48, sector as u32, &[(8 << 20, TargetFs::Fat32), (16 << 20, TargetFs::Ntfs), (0, TargetFs::Ext4)]);
                let want = entries_of(dev.as_ref());
                let guid0 = read_disk(dev.as_ref()).unwrap().primary.unwrap().ph.h.disk_guid;
                let lbas = dev.size() / sector;
                // Put recognisable data in every partition: repair must not touch it.
                for e in &want {
                    dev.write_at(e.first_lba * sector, &[0xA5; 4096]).unwrap();
                    dev.write_at(e.last_lba * sector, &vec![0x5A; sector as usize]).unwrap();
                }
                corrupt(&dev, sector, lbas);
                let found = codes(dev.as_ref());
                assert!(found.contains(code), "{sector} {code}: {found:?}");
                let r = run_on(dev.clone(), DoctorAction::Repair).unwrap_or_else(|e| panic!("{sector} {code}: {e}"));
                assert!(!r["actions"].as_array().unwrap().is_empty(), "{code}");
                assert_eq!(r["after"]["healthy"], true, "{sector} {code}: {r}");
                assert_eq!(entries_of(dev.as_ref()), want, "{code}");
                assert_eq!(read_disk(dev.as_ref()).unwrap().primary.unwrap().ph.h.disk_guid, guid0);
                for e in &want {
                    assert_eq!(dev.read_vec(e.first_lba * sector, 4096).unwrap(), vec![0xA5; 4096]);
                    assert_eq!(dev.read_vec(e.last_lba * sector, sector as usize).unwrap(), vec![0x5A; sector as usize]);
                }
                assert_eq!(parse_partitions(dev.as_ref()).unwrap().len(), 3);
            }
        }
    }

    #[test]
    fn image_written_to_a_larger_disk_gets_its_backup_moved_to_the_end() {
        let small = gpt_disk(32, 512, &[(8 << 20, TargetFs::Fat32), (0, TargetFs::Exfat)]);
        let want = entries_of(small.as_ref());
        let mut img = small.mem.snapshot();
        img.resize(64 << 20, 0);
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(img));
        let c = codes(dev.as_ref());
        assert!(c.contains(&"gpt_backup_not_at_end") && c.contains(&"pmbr_size"), "{c:?}");
        let r = run_on(dev.clone(), DoctorAction::Repair).unwrap();
        assert_eq!(r["after"]["healthy"], true, "{r}");
        let d = read_disk(dev.as_ref()).unwrap();
        let p = d.primary.as_ref().unwrap();
        assert_eq!(p.ph.h.alt_lba, (64 << 20) / 512 - 1);
        assert_eq!(p.ph.h.last_usable, (64 << 20) / 512 - 34, "usable area grows to the new end");
        assert_eq!(d.backup.as_ref().unwrap().lba, (64 << 20) / 512 - 1);
        assert_eq!(entries_of(dev.as_ref()), want);
        // The old backup header (at the old end, now free space) is gone.
        assert_ne!(&dev.read_vec(((32 << 20) / 512 - 1) * 512, 8).unwrap()[..], b"EFI PART");
    }

    #[test]
    fn image_written_to_a_smaller_disk_is_repaired_only_when_everything_fits() {
        // Partitions leave 8 MiB free at the end: the disk can lose up to that much.
        let big = gpt_disk(48, 512, &[(8 << 20, TargetFs::Fat32), (24 << 20, TargetFs::Exfat)]);
        let want = entries_of(big.as_ref());
        let mut img = big.mem.snapshot();
        img.truncate(40 << 20);
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::from_vec(img.clone()));
        assert!(codes(dev.as_ref()).contains(&"gpt_beyond_end"));
        let r = run_on(dev.clone(), DoctorAction::Repair).unwrap();
        assert_eq!(r["after"]["healthy"], true, "{r}");
        assert_eq!(entries_of(dev.as_ref()), want);
        // Cutting into a partition: refused, nothing written.
        let mut img = big.mem.snapshot();
        img.truncate(30 << 20);
        let dev = Arc::new(MemDev::from_vec(img.clone()));
        assert!(run_on(dev.clone(), DoctorAction::Repair).is_err());
        assert!(dev.snapshot() == img);
    }

    #[test]
    fn ambiguous_or_hopeless_tables_are_refused_untouched() {
        // Both copies valid but different.
        let dev = gpt_disk(32, 512, &[(8 << 20, TargetFs::Fat32), (0, TargetFs::Exfat)]);
        let d = read_disk(dev.as_ref()).unwrap();
        let b = d.backup.unwrap();
        let mut arr = b.array.unwrap();
        arr[32] ^= 0x08; // move partition 1's start in the backup only
        let mut h = b.ph.h.clone();
        h.entries_crc = layout::crc32(&arr);
        dev.write_at(h.entries_lba * 512, &arr).unwrap();
        dev.write_at(b.lba * 512, &h.to_bytes(512)).unwrap();
        assert!(codes(dev.as_ref()).contains(&"gpt_mismatch"));
        let img = dev.mem.snapshot();
        let e = run_on(dev.clone(), DoctorAction::Repair).unwrap_err();
        assert!(e.to_string().contains("different") || e.to_string().contains("diverse"), "{e}");
        assert!(dev.mem.snapshot() == img);
        // Both copies damaged differently.
        let dev = gpt_disk(32, 512, &[(0, TargetFs::Exfat)]);
        dev.write_at(512 + 40, &[1]).unwrap();
        dev.write_at(((32 << 20) / 512 - 1) * 512 + 48, &[2]).unwrap();
        assert!(codes(dev.as_ref()).contains(&"gpt_no_valid_copy"));
        assert!(run_on(dev.clone(), DoctorAction::Repair).is_err());
        // Overlapping entries (valid CRCs): reported, not "repaired".
        let dev = gpt_disk(32, 512, &[(8 << 20, TargetFs::Fat32), (0, TargetFs::Exfat)]);
        let d = read_disk(dev.as_ref()).unwrap();
        let p = d.primary.unwrap();
        let mut es = p.entries();
        es[1].first_lba = es[0].last_lba - 10;
        let arr = layout::gpt_array(&es).unwrap();
        let h = GptHeader { entries_crc: layout::crc32(&arr), ..p.ph.h.clone() };
        layout::write_gpt_raw(dev.as_ref(), 512, &h, &arr).unwrap();
        assert!(codes(dev.as_ref()).contains(&"gpt_overlap"));
        // …even with a broken backup header that alone would be repairable.
        dev.write_at(((32 << 20) / 512 - 1) * 512 + 48, &[2]).unwrap();
        let img = dev.mem.snapshot();
        assert!(run_on(dev.clone(), DoctorAction::Repair).is_err());
        assert!(dev.mem.snapshot() == img);
        // An MBR with partitions next to a GPT without 0xEE.
        let dev = gpt_disk(32, 512, &[(0, TargetFs::Exfat)]);
        let m = layout::build_mbr(&[], [1, 2, 3, 4], &[MbrEntry { boot: 0, kind: 0x0C, start: 2048, count: 4096 }]);
        dev.write_at(0, &m).unwrap();
        assert!(codes(dev.as_ref()).contains(&"stale_gpt"));
        assert!(run_on(dev.clone(), DoctorAction::Repair).unwrap()["actions"].as_array().unwrap().is_empty());
    }

    #[test]
    fn identical_copies_with_wrong_crcs_get_their_crcs_recomputed() {
        let dev = gpt_disk(32, 512, &[(8 << 20, TargetFs::Fat32), (0, TargetFs::Exfat)]);
        let want = entries_of(dev.as_ref());
        let lbas = (32u64 << 20) / 512;
        // An editor changed the name of partition 1 in both arrays without updating the CRCs.
        for lba in [2, lbas - 33] {
            dev.write_at(lba * 512 + 56, &[b'X', 0]).unwrap();
        }
        let c = codes(dev.as_ref());
        assert!(c.contains(&"gpt_crc_only"), "{c:?}");
        run_on(dev.clone(), DoctorAction::Repair).unwrap();
        let got = entries_of(dev.as_ref());
        assert_eq!(got[0].name, "Xart0");
        assert_eq!(got.iter().map(|e| (e.first_lba, e.last_lba)).collect::<Vec<_>>(), want.iter().map(|e| (e.first_lba, e.last_lba)).collect::<Vec<_>>());
    }

    #[test]
    fn hybrid_mbr_is_reported_and_left_alone() {
        let dev = Arc::new(MemDev::new(64 << 20));
        let parts = [NewPart { size: Some(8 << 20), kind: PartType::EfiSystem, name: "EFI".into(), bootable: false }, NewPart { size: None, kind: PartType::Fs(TargetFs::Fat32), name: "D".into(), bootable: true }];
        layout::write_table(dev.as_ref(), Scheme::Gpt, &parts, &TableOptions { hybrid: vec![2], ..Default::default() }).unwrap();
        let r = run_on(dev.clone(), DoctorAction::Check).unwrap();
        assert_eq!(r["scheme"], "hybrid");
        let c = codes(dev.as_ref());
        assert!(c.contains(&"hybrid_mbr") && !c.contains(&"hybrid_mismatch"), "{c:?}");
        // A hybrid entry that matches nothing is an error.
        dev.write_at(446 + 16 + 8, &4000u32.to_le_bytes()).unwrap();
        assert!(codes(dev.as_ref()).contains(&"hybrid_mismatch"));
    }

    #[test]
    fn mbr_problems_are_reported() {
        let dev = Arc::new(MemDev::new(32 << 20));
        let lbas = (32u32 << 20) / 512;
        let m = layout::build_mbr(
            &[],
            [9; 4],
            &[
                MbrEntry { boot: 0x80, kind: 0x0C, start: 2048, count: 8192 },
                MbrEntry { boot: 0x80, kind: 0x83, start: 8000, count: 4096 },
                MbrEntry { boot: 0x12, kind: 0x07, start: lbas - 100, count: 4096 },
            ],
        );
        dev.write_at(0, &m).unwrap();
        let c = codes(dev.as_ref());
        for want in ["mbr_overlap", "mbr_beyond_end", "mbr_bad_boot_flag", "mbr_multiple_active"] {
            assert!(c.contains(&want), "{want}: {c:?}");
        }
        assert!(run_on(dev.clone(), DoctorAction::ConvertToGpt).is_err());
        // A superfloppy and an empty disk.
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(40 << 20));
        crate::vfs::fat_fs::format(dev.clone(), crate::vfs::FsType::Fat32, "X").unwrap();
        let r = run_on(dev.clone(), DoctorAction::Check).unwrap();
        assert_eq!(r["scheme"], "none");
        assert_eq!(r["healthy"], true);
        assert!(run_on(dev.clone(), DoctorAction::ConvertToGpt).is_err());
        assert!(run_on(Arc::new(MemDev::new(1 << 20)), DoctorAction::ConvertToMbr).is_err());
    }

    /// An MBR disk with an extended partition and logical partitions (as fdisk lays them out).
    fn mbr_with_logicals(dev: &dyn BlockDev, lbas: u64) {
        let ext_start = 40960u64;
        let m = layout::build_mbr(
            &[0xFA, 0x33, 0xC0],
            [0xDE, 0xAD, 0xBE, 0xEF],
            &[MbrEntry { boot: 0x80, kind: 0x0C, start: 2048, count: 38912 }, MbrEntry { boot: 0, kind: 0x05, start: ext_start as u32, count: (lbas - ext_start - 2048) as u32 }],
        );
        dev.write_at(0, &m).unwrap();
        // Logical 5 at ext+2048 (8 MiB, Linux), logical 6 after a 1 MiB gap (exFAT/NTFS).
        let l5 = (ext_start + 2048, 16384u64);
        let ebr2 = l5.0 + l5.1;
        let l6 = (ebr2 + 2048, 16384u64);
        let e1 = layout::build_mbr(&[], [0; 4], &[MbrEntry { boot: 0, kind: 0x83, start: 2048, count: l5.1 as u32 }, MbrEntry { boot: 0, kind: 0x05, start: (ebr2 - ext_start) as u32, count: (l6.1 + 2048) as u32 }]);
        dev.write_at(ext_start * 512, &e1).unwrap();
        let e2 = layout::build_mbr(&[], [0; 4], &[MbrEntry { boot: 0, kind: 0x07, start: 2048, count: l6.1 as u32 }]);
        dev.write_at(ebr2 * 512, &e2).unwrap();
    }

    #[test]
    fn mbr_with_logical_partitions_converts_to_gpt_and_back() {
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(64 << 20));
        let lbas = (64u64 << 20) / 512;
        mbr_with_logicals(dev.as_ref(), lbas);
        let before: Vec<(u64, u64)> = parse_partitions(dev.as_ref()).unwrap().iter().map(|p| (p.start, p.size)).collect();
        assert_eq!(before.len(), 3);
        assert_eq!(codes(dev.as_ref()), Vec::<&str>::new(), "a healthy MBR");
        let r = run_on(dev.clone(), DoctorAction::ConvertToGpt).unwrap();
        assert_eq!(r["after"]["scheme"], "gpt");
        assert_eq!(r["after"]["healthy"], true, "{r}");
        let after = parse_partitions(dev.as_ref()).unwrap();
        assert_eq!(after.iter().map(|p| (p.start, p.size)).collect::<Vec<_>>(), before);
        assert_eq!(after[0].type_name, "Microsoft basic data");
        assert_eq!(after[1].type_name, "Linux filesystem");
        let sector0 = dev.read_vec(0, 512).unwrap();
        assert_eq!(&sector0[..3], &[0xFA, 0x33, 0xC0], "boot code kept");
        assert_eq!(&sector0[440..444], &[0xDE, 0xAD, 0xBE, 0xEF], "disk signature kept");
        let g = read_disk(dev.as_ref()).unwrap();
        assert_eq!(g.good_gpt().unwrap().entries()[0].attrs, 1 << 2, "active → legacy BIOS bootable");
        // Back to MBR: 3 partitions fit as primaries.
        let r = run_on(dev.clone(), DoctorAction::ConvertToMbr).unwrap();
        assert_eq!(r["after"]["scheme"], "mbr", "{r}");
        let back = parse_partitions(dev.as_ref()).unwrap();
        assert_eq!(back.iter().map(|p| (p.start, p.size)).collect::<Vec<_>>(), before);
        assert_eq!(back[1].type_id, "0x83");
        assert_eq!(layout::mbr_entries(&dev.read_vec(0, 512).unwrap())[0].boot, 0x80);
        let d = read_disk(dev.as_ref()).unwrap();
        assert!(d.primary.is_none() && d.backup.is_none(), "GPT erased");
    }

    #[test]
    fn conversions_refuse_what_does_not_fit() {
        // Partition starting at sector 1 (no room for the primary GPT).
        let lbas = (32u64 << 20) / 512;
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(32 << 20));
        dev.write_at(0, &layout::build_mbr(&[], [1; 4], &[MbrEntry { boot: 0, kind: 0x0C, start: 1, count: 8192 }])).unwrap();
        let e = run_on(dev.clone(), DoctorAction::ConvertToGpt).unwrap_err().to_string();
        assert!(e.contains("34"), "{e}");
        // Partition reaching the last sector (no room for the backup GPT).
        dev.write_at(0, &layout::build_mbr(&[], [1; 4], &[MbrEntry { boot: 0, kind: 0x0C, start: 2048, count: (lbas - 2048) as u32 }])).unwrap();
        let e = run_on(dev.clone(), DoctorAction::ConvertToGpt).unwrap_err().to_string();
        assert!(e.contains("33"), "{e}");
        // …but 33 free sectors at the end are enough.
        dev.write_at(0, &layout::build_mbr(&[], [1; 4], &[MbrEntry { boot: 0, kind: 0x0C, start: 34, count: (lbas - 34 - 33) as u32 }])).unwrap();
        run_on(dev.clone(), DoctorAction::ConvertToGpt).unwrap();
        // Unknown MBR type / LDM.
        dev.write_at(0, &layout::build_mbr(&[], [1; 4], &[MbrEntry { boot: 0, kind: 0x9F, start: 2048, count: 8192 }])).unwrap();
        assert!(run_on(dev.clone(), DoctorAction::ConvertToGpt).unwrap_err().to_string().contains("0x9F"));
        dev.write_at(0, &layout::build_mbr(&[], [1; 4], &[MbrEntry { boot: 0, kind: 0x42, start: 2048, count: 8192 }])).unwrap();
        assert!(run_on(dev.clone(), DoctorAction::ConvertToGpt).is_err());
        // GPT → MBR: 5 partitions, a partition beyond 2 TiB, an MSR, a damaged GPT.
        let five = gpt_disk(64, 512, &[(4 << 20, TargetFs::Ext4); 5]);
        assert!(run_on(five.clone(), DoctorAction::ConvertToMbr).unwrap_err().to_string().contains('5'));
        let msr = Arc::new(MemDev::new(64 << 20));
        let parts = [NewPart { size: Some(16 << 20), kind: PartType::Raw { mbr: 0, gpt: guid(guids::MS_RESERVED) }, name: "MSR".into(), bootable: false }, NewPart { size: None, kind: PartType::Fs(TargetFs::Ntfs), name: "".into(), bootable: false }];
        layout::write_table(msr.as_ref(), Scheme::Gpt, &parts, &TableOptions::default()).unwrap();
        assert!(run_on(msr.clone(), DoctorAction::ConvertToMbr).unwrap_err().to_string().contains("MSR"));
        let bad = gpt_disk(32, 512, &[(0, TargetFs::Ext4)]);
        bad.write_at(512 + 40, &[1]).unwrap();
        assert!(run_on(bad.clone(), DoctorAction::ConvertToMbr).is_err());
        assert!(run_on(bad.clone(), DoctorAction::ConvertToGpt).is_err(), "already GPT");
        // > 2 TiB: a sparse table-only check through the planner on a fake 3 TiB disk.
        let mut d = read_disk(gpt_disk(32, 512, &[(0, TargetFs::Ext4)]).as_ref()).unwrap();
        let g = d.primary.as_mut().unwrap();
        let mut es = g.entries();
        es[0].last_lba = 3u64 << 31;
        g.array = Some(layout::gpt_array(&es).unwrap());
        g.ph.h.entries_crc = layout::crc32(g.array.as_ref().unwrap());
        g.ph.h.last_usable = (3u64 << 31) + 10;
        d.lbas = (3u64 << 31) + 100;
        d.size = d.lbas * 512;
        d.backup = None;
        let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new(1 << 20));
        assert!(plan_to_mbr(&d, &dev).is_err());
    }

    #[test]
    fn writes_into_partitions_are_refused_by_the_guard() {
        let w = vec![Write { off: 1 << 20, data: vec![0; 512], what: "x".into() }];
        assert!(check_writes(&w, &[(1 << 20, 2 << 20)], 4 << 20).is_err());
        assert!(check_writes(&w, &[(2 << 20, 3 << 20)], 4 << 20).is_ok());
        assert!(check_writes(&w, &[], (1 << 20) + 100).is_err(), "beyond the end");
    }

    #[test]
    fn the_parser_survives_garbage_tables() {
        let mut r = Rng(0xD0C);
        for i in 0..400 {
            let sector = if i % 3 == 0 { 4096 } else { 512 };
            let dev = gpt_disk(4, sector, &[(1 << 20, TargetFs::Fat16), (0, TargetFs::Ext2)]);
            // Random bytes over sector 0, the primary header and the start of the array, and
            // sometimes an extended partition pointing anywhere.
            for _ in 0..1 + r.below(40) {
                let off = r.below(3 * sector as u64 + 512);
                dev.write_at(off, &[r.next() as u8]).unwrap();
            }
            if r.below(4) == 0 {
                let m = layout::build_mbr(&[], [0; 4], &[MbrEntry { boot: 0, kind: 0x05, start: r.below(1 << 20) as u32, count: r.next() as u32 }]);
                dev.write_at(0, &m).unwrap();
            }
            if r.below(4) == 0 {
                dev.write_at(sector as u64 + 80, &(r.next() as u32).to_le_bytes()).unwrap();
            }
            let Ok(d) = read_disk(dev.as_ref()) else { continue };
            let _ = report(&d);
            let _ = plan_repair(&d);
            let _ = plan_to_gpt(&d);
            let dd: Arc<dyn BlockDev> = dev.clone();
            let _ = plan_to_mbr(&d, &dd);
        }
    }

    /// Property: random GPT layouts with a random structure destroyed are repaired back to the
    /// exact original entries, and the partitions' bytes are never touched.
    #[test]
    fn random_corruptions_are_repaired() {
        let mut r = Rng(0xFEED);
        let mut repaired = 0;
        for round in 0..150 {
            let sector = if r.below(3) == 0 { 4096u64 } else { 512 };
            let n = 1 + r.below(5) as usize;
            let parts: Vec<(u64, TargetFs)> = (0..n).map(|i| (if i == n - 1 && r.below(2) == 0 { 0 } else { (1 + r.below(6)) << 20 }, TargetFs::Ext4)).collect();
            let dev = gpt_disk(48, sector as u32, &parts);
            let want = entries_of(dev.as_ref());
            let lbas = dev.size() / sector;
            let es = 16384 / sector;
            let data: Vec<u8> = (0..4096).map(|_| r.next() as u8).collect();
            for e in &want {
                dev.write_at(e.first_lba * sector, &data).unwrap();
            }
            // Pick what to destroy: one copy's header or array (bytes or whole), or sector 0.
            let what = r.below(5);
            let (lba, len) = match what {
                0 => (1, sector),
                1 => (2, es * sector),
                2 => (lbas - 1, sector),
                3 => (lbas - 1 - es, es * sector),
                _ => (0, 512),
            };
            if r.below(2) == 0 {
                dev.write_at(lba * sector, &vec![0u8; len as usize]).unwrap();
            } else {
                for _ in 0..1 + r.below(8) {
                    let o = lba * sector + r.below(len.min(if what % 2 == 0 { 92 } else { len }));
                    let b = dev.read_vec(o, 1).unwrap()[0];
                    dev.write_at(o, &[b ^ (1 + r.below(255) as u8)]).unwrap();
                }
            }
            let res = run_on(dev.clone(), DoctorAction::Repair);
            let res = res.unwrap_or_else(|e| panic!("round {round} (what {what}, sector {sector}): {e}"));
            assert_eq!(res["after"]["healthy"], true, "round {round}: {res}");
            if !res["actions"].as_array().unwrap().is_empty() {
                repaired += 1;
            }
            assert_eq!(entries_of(dev.as_ref()), want, "round {round}");
            for e in &want {
                assert_eq!(dev.read_vec(e.first_lba * sector, 4096).unwrap(), data, "round {round}: data touched");
            }
        }
        // Flips in the MBR's boot code area change nothing that needs repairing; the rest must.
        assert!(repaired > 100, "only {repaired} rounds needed a repair");
    }

    /// Property: random MBR layouts convert to GPT and back with identical extents.
    #[test]
    fn random_mbr_layouts_round_trip_through_gpt() {
        let mut r = Rng(0xABCD);
        let kinds = [0x0C, 0x07, 0x83, 0x82, 0xEF, 0x8E];
        for round in 0..100 {
            let sector = if r.below(4) == 0 { 4096u64 } else { 512 };
            let dev = Arc::new(SectorDev::new(64 << 20, sector as u32));
            let lbas = dev.size() / sector;
            let n = 1 + r.below(4);
            let mut pos = 34 + r.below(4000) * 512 / sector;
            let mut es = vec![];
            for _ in 0..n {
                let count = 1 + r.below(lbas / 8);
                if pos + count + 33 > lbas {
                    break;
                }
                es.push(MbrEntry { boot: if r.below(4) == 0 { 0x80 } else { 0 }, kind: kinds[r.below(kinds.len() as u64) as usize], start: pos as u32, count: count as u32 });
                pos += count + r.below(100);
            }
            if es.is_empty() {
                continue;
            }
            dev.write_at(0, &layout::build_mbr(&[], [7; 4], &es)).unwrap();
            let before: Vec<(u64, u64, String)> = parse_partitions(dev.as_ref()).unwrap().into_iter().map(|p| (p.start, p.size, p.type_id)).collect();
            let d: Arc<dyn BlockDev> = dev.clone();
            run_on(d.clone(), DoctorAction::ConvertToGpt).unwrap_or_else(|e| panic!("round {round}: {e}"));
            assert_eq!(codes(dev.as_ref()), Vec::<&str>::new(), "round {round}");
            run_on(d.clone(), DoctorAction::ConvertToMbr).unwrap_or_else(|e| panic!("round {round}: {e}"));
            let after: Vec<(u64, u64, String)> = parse_partitions(dev.as_ref()).unwrap().into_iter().map(|p| (p.start, p.size, p.type_id)).collect();
            // Extents identical; types identical except FAT32 / NTFS data partitions, whose MBR
            // type comes from the (here absent) filesystem.
            assert_eq!(after.len(), before.len());
            for (a, b) in after.iter().zip(&before) {
                assert_eq!((a.0, a.1), (b.0, b.1), "round {round}");
                if !matches!(b.2.as_str(), "0x0C" | "0x07") {
                    assert_eq!(a.2, b.2, "round {round}");
                }
            }
            let m = layout::mbr_entries(&dev.read_vec(0, 512).unwrap());
            let mut sorted = es.clone();
            sorted.sort_by_key(|e| e.start);
            for (x, y) in m.iter().zip(&sorted) {
                assert_eq!(x.boot, y.boot, "round {round}: active flag survives both ways");
            }
        }
    }

    #[test]
    fn check_only_reads_and_files_are_never_resized() {
        let dir = std::env::temp_dir().join(format!("lumendisk-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("d.img");
        std::fs::write(&p, vec![0u8; 4 << 20]).unwrap();
        let r = run(&DoctorRequest { target: TargetSpec::File { path: p.clone(), size: Some(1 << 20) }, action: DoctorAction::Check }, &ctx()).unwrap();
        assert_eq!(r["scheme"], "none");
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 4 << 20);
        assert!(run(&DoctorRequest { target: TargetSpec::File { path: dir.join("missing.img"), size: None }, action: DoctorAction::Repair }, &ctx()).is_err());
        assert!(!dir.join("missing.img").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
