//! A quick look at an ISO 9660 image: volume label, El Torito, hybrid partition tables.
//! (The full reader, with directory trees, is `crate::iso`.)
//!
//! References: ECMA-119 (ISO 9660) §8 volume descriptors from sector 16 (2048-byte sectors):
//! type byte, "CD001", version; Primary Volume Descriptor: volume identifier @40 (32 bytes),
//! volume space size @80 (u32 LE), logical block size @128 (u16 LE). Joliet = Supplementary
//! Volume Descriptor with escape sequence "%/@", "%/C" or "%/E" @88. El Torito 1.0: Boot Record
//! (type 0) with "EL TORITO SPECIFICATION" @7 and the boot catalog sector @71 (u32 LE); catalog
//! validation entry (platform @1, 0x55AA @30) followed by entries and section headers
//! (0x90 / 0x91, platform @1; 0xEF = EFI). ECMA-167 / UDF: "BEA01", "NSR02"/"NSR03" volume
//! structure descriptors in the same area. "Hybrid" (isohybrid / xorriso): an MBR (0x55AA
//! @510 with a partition entry) and / or a GPT header ("EFI PART" @512) in the system area,
//! so the same bytes boot from a USB disk.

use serde::Serialize;

use crate::block::BlockDev;

#[derive(Clone, Debug, Default, Serialize)]
pub struct IsoInfo {
    pub label: String,
    /// Volume space size × logical block size.
    pub size: u64,
    pub joliet: bool,
    pub udf: bool,
    pub el_torito: bool,
    /// The El Torito catalog has an EFI (platform 0xEF) entry.
    pub efi_boot: bool,
    /// Partition tables found in the system area ("mbr", "gpt").
    pub hybrid_tables: Vec<&'static str>,
    /// Bootable when written byte-for-byte to a USB disk (DD mode).
    pub hybrid: bool,
}

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

/// None if `dev` is not an ISO 9660 / UDF image.
pub fn probe(dev: &dyn BlockDev) -> Option<IsoInfo> {
    let mut info = IsoInfo::default();
    let mut is_iso = false;
    let mut catalog = None;
    for i in 16..80u64 {
        let Ok(d) = dev.read_vec(i * 2048, 2048) else { break };
        match &d[1..6] {
            b"CD001" => {
                is_iso = true;
                match d[0] {
                    0 if d[7..30] == *b"EL TORITO SPECIFICATION" => {
                        info.el_torito = true;
                        catalog = Some(u64::from(le32(&d, 71)));
                    }
                    1 => {
                        info.label = String::from_utf8_lossy(&d[40..72]).trim_end_matches([' ', '\0']).to_string();
                        info.size = u64::from(le32(&d, 80)) * u64::from(le16(&d, 128));
                    }
                    2 if d[88] == b'%' && d[89] == b'/' && matches!(d[90], b'@' | b'C' | b'E') => {
                        info.joliet = true;
                        // Joliet labels are UCS-2 BE and keep their case: nicer than the PVD's.
                        let u: Vec<u16> = d[40..72].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
                        let l = String::from_utf16_lossy(&u).trim_end().to_string();
                        if !l.is_empty() {
                            info.label = l;
                        }
                    }
                    _ => {}
                }
            }
            b"BEA01" | b"TEA01" => {}
            b"NSR02" | b"NSR03" => {
                info.udf = true;
                is_iso = true;
            }
            _ if d.iter().all(|&b| b == 0) => break,
            _ => {
                if !is_iso {
                    return None;
                }
            }
        }
    }
    if !is_iso {
        return None;
    }
    if let Some(lba) = catalog {
        if let Ok(c) = dev.read_vec(lba.saturating_mul(2048), 2048) {
            if c[0] == 1 && c[30] == 0x55 && c[31] == 0xAA {
                info.efi_boot = c[1] == 0xEF;
                // Section headers (0x90 more follow, 0x91 last) after the initial entry.
                let mut o = 64;
                while o + 32 <= c.len() {
                    match c[o] {
                        0x90 | 0x91 => {
                            if c[o + 1] == 0xEF {
                                info.efi_boot = true;
                            }
                            let n = le16(&c, o + 2) as usize;
                            let last = c[o] == 0x91;
                            o += 32 * (n + 1);
                            if last {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
    }
    if let Ok(s) = dev.read_vec(0, 1024) {
        if s[510] == 0x55 && s[511] == 0xAA && (0..4).any(|i| s[446 + i * 16 + 4] != 0) {
            info.hybrid_tables.push("mbr");
        }
        if &s[512..520] == b"EFI PART" {
            info.hybrid_tables.push("gpt");
        }
    }
    info.hybrid = !info.hybrid_tables.is_empty();
    Some(info)
}
