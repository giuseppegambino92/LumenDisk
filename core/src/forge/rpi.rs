//! Raspberry Pi / SBC first-boot customisation of a written image: hostname, user and
//! password, SSH, Wi-Fi, locale (firstrun.sh / userconf.txt, or cloud-init).
//!
//! # Detection ([`detect`])
//! The boot partition is the first FAT partition (MBR or GPT) holding `config.txt` and one of
//! `cmdline.txt`, `bootcode.bin`, `start*.elf`. The OS comes from the boot partition's
//! `issue.txt` ("Raspberry Pi reference …" = Raspberry Pi OS) and from `/etc/os-release` on the
//! ext4 root partition, read with LumenDisk's own ext4 driver (symlinks followed). The method:
//! - **cloud-init** when the boot partition has `user-data`, `meta-data` or `network-config`
//!   (Ubuntu for Raspberry Pi; Raspberry Pi OS since the trixie releases after 2025-10-01,
//!   Imager's `cloudinit-rpi` format);
//! - **firstrun** for Raspberry Pi OS without them (bookworm and older, and the first trixie
//!   release: Imager's `systemd` format);
//! - **none** otherwise ([`customize`] refuses).
//!
//! # What is written ([`customize`]), following Raspberry Pi Imager **2.0.12**
//! (github.com/raspberrypi/rpi-imager, `src/customization_generator.cpp`, `src/downloadthread.cpp`,
//! `src/cmdline_params.h`, Apache-2.0; the generated text is reproduced, not the code):
//! - firstrun: `firstrun.sh` (Imager's script: `imager_custom` set_hostname / enable_ssh /
//!   set_wlan / set_keymap / set_timezone with their fallbacks, `userconf` for the account, the
//!   bullseye-era `wpa_supplicant.conf` fallback), `cmdline.txt` + `cfg80211.ieee80211_regdom=CC
//!   systemd.run=/boot/firstrun.sh systemd.run_success_action=reboot
//!   systemd.unit=kernel-command-line.target`. In addition to Imager: `userconf.txt`
//!   (`user:$6$…`, Raspberry Pi OS's documented headless account file) and the empty `ssh` file,
//!   so the account and SSH still come up if the firstrun step never runs; firstrun.sh removes
//!   `userconf.txt` once it has applied the account (the `ssh` file is consumed by
//!   `sshswitch.service`), and sets the locale (raspi-config) when one is given. Bookworm's
//!   initramfs (`imager_fixup`) rewrites `/boot/` → `/boot/firmware/` in cmdline.txt and cuts
//!   firstrun.sh at its `rm …firstrun.sh` line: everything LumenDisk adds comes before it.
//! - cloud-init: `user-data` (`#cloud-config`: hostname, `user:` — the distro's default user,
//!   renamed, keeping its groups — with the `$6$` hash, `lock_passwd: false`, `sudo: null`,
//!   `ssh_pwauth`, `ssh_authorized_keys`, timezone, keyboard, locale, `runcmd` enabling ssh),
//!   `network-config` (netplan v2: eth0 DHCP + `wifis.wlan0` with `regulatory-domain`, `hidden`),
//!   `meta-data` (a fresh `instance-id`), and on Raspberry Pi OS ` ds=nocloud;i=<id>` (+ regdom)
//!   on `cmdline.txt`. Ubuntu keeps its cmdline (in `current/`, untouched).
//!
//! **Merging**: existing files are edited rather than replaced where that is safe. `user-data`:
//! top-level keys LumenDisk sets are replaced, `packages` / `runcmd` lists are extended, the
//! rest (and comments) is kept; when a user is configured, `chpasswd` / `password` are dropped
//! (Ubuntu's stock `chpasswd: expire: true` would otherwise expire the new password). A
//! `user-data` that isn't a block-style `#cloud-config` document is replaced (and said so in the
//! result). `meta-data` keeps every line but the instance id. `network-config` is replaced when
//! Wi-Fi is set, untouched otherwise. `cmdline.txt` stays one line; parameters LumenDisk adds
//! are removed first, so customising twice doesn't stack them.
//!
//! Every value is validated first (nothing is written if one is wrong) and then quoted for
//! where it goes (POSIX shell single quotes, YAML double quotes). Passwords are hashed with
//! SHA-512 crypt (`$6$`, random salt) and never logged; Wi-Fi keys are never logged.
//!
//! Owner: the Raspberry Pi / utilities workstream.

use std::collections::VecDeque;
use std::sync::Arc;

use serde_json::{json, Value};

use super::crypt;
use super::job::{JobCtx, Phase};
use super::plan::RpiOptions;
use super::{ForgeError, ForgeResult};
use crate::block::{self, BlockDev, PartitionDev};
use crate::i18n::t;
use crate::vfs::{self, FileKind, FileSystem, FsError, FsType, MountDefaults};

/// The Raspberry Pi Imager release whose output LumenDisk reproduces.
pub const IMAGER_VERSION: &str = "2.0.12";

/// Text files read from an image are small; anything bigger is not what we look for.
const MAX_TEXT: u64 = 1 << 20;
const MAX_PARTITIONS: usize = 16;
const MAX_SYMLINKS: u32 = 8;

/// What [`detect`] found on an image / disk (serialised into the inspect result).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct RpiInfo {
    /// "Raspberry Pi OS (bookworm)", "Ubuntu 24.04 (Raspberry Pi)"…
    pub os: String,
    /// "firstrun" (Raspberry Pi OS ≤ bookworm), "cloud-init" (Ubuntu, Raspberry Pi OS trixie+),
    /// "none" (a Raspberry Pi boot partition, but no known way to customise the OS).
    pub method: String,
    /// "raspios", "ubuntu", "other".
    pub family: String,
    /// `VERSION_CODENAME` of the root filesystem's os-release, when readable.
    pub codename: Option<String>,
    /// Index (1-based) of the FAT boot partition.
    pub boot_partition: u32,
    /// Index of the ext root partition, when one was found.
    pub root_partition: Option<u32>,
}

// ───────────────────────────── reading the image ─────────────────────────────

/// Resolve `path` (absolute, `/`-separated) in `fs`, following symbolic links (at most
/// [`MAX_SYMLINKS`]). None when a component is missing or the links loop.
fn resolve(fs: &mut dyn FileSystem, path: &str) -> ForgeResult<Option<(vfs::Ino, FileKind)>> {
    let root = fs.root_ino();
    let mut chain = vec![root];
    let mut todo: VecDeque<String> = path.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
    let mut hops = 0;
    let mut kind = FileKind::Dir;
    while let Some(c) = todo.pop_front() {
        if c == "." {
            continue;
        }
        if c == ".." {
            if chain.len() > 1 {
                chain.pop();
            }
            kind = FileKind::Dir;
            continue;
        }
        if kind != FileKind::Dir {
            return Ok(None);
        }
        let dir = *chain.last().unwrap_or(&root);
        let ino = match fs.lookup(dir, &c) {
            Ok(i) => i,
            Err(FsError::NotFound) | Err(FsError::NotDir) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let attr = fs.getattr(ino)?;
        if attr.kind == FileKind::Symlink {
            hops += 1;
            if hops > MAX_SYMLINKS {
                return Ok(None);
            }
            let target = fs.readlink(ino)?;
            if target.len() > 4096 {
                return Ok(None);
            }
            if target.starts_with('/') {
                chain.truncate(1);
            }
            for comp in target.split('/').filter(|c| !c.is_empty()).rev() {
                todo.push_front(comp.to_string());
            }
            kind = FileKind::Dir;
            continue;
        }
        kind = attr.kind;
        chain.push(ino);
    }
    Ok(Some((*chain.last().unwrap_or(&root), kind)))
}

/// A whole regular file of at most `max` bytes; None if missing, not a file, or bigger.
fn read_small(fs: &mut dyn FileSystem, path: &str, max: u64) -> ForgeResult<Option<Vec<u8>>> {
    let Some((ino, kind)) = resolve(fs, path)? else { return Ok(None) };
    if kind != FileKind::File {
        return Ok(None);
    }
    let size = fs.getattr(ino)?.size;
    if size > max {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(size as usize);
    while (out.len() as u64) < size {
        let want = (size - out.len() as u64).min(256 << 10) as u32;
        let chunk = fs.read(ino, out.len() as u64, want)?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    out.truncate(size as usize);
    Ok(Some(out))
}

fn read_text(fs: &mut dyn FileSystem, path: &str) -> ForgeResult<Option<String>> {
    Ok(read_small(fs, path, MAX_TEXT)?.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

fn exists(fs: &mut dyn FileSystem, name: &str) -> bool {
    matches!(resolve(fs, name), Ok(Some(_)))
}

/// `KEY=value` pairs of an os-release file (quotes and backslash escapes removed).
fn os_release(text: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in text.lines().take(200) {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        let v = if v.len() >= 2 && (v.starts_with('"') && v.ends_with('"') || v.starts_with('\'') && v.ends_with('\'')) { &v[1..v.len() - 1] } else { v };
        let mut val = String::new();
        let mut esc = false;
        for ch in v.chars() {
            if esc {
                val.push(ch);
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else {
                val.push(ch);
            }
        }
        out.push((k.trim().to_string(), val.chars().filter(|c| !c.is_control()).take(200).collect()));
    }
    out
}

fn get<'a>(kv: &'a [(String, String)], k: &str) -> Option<&'a str> {
    kv.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

fn fat_of(dev: &Arc<dyn BlockDev>) -> bool {
    matches!(vfs::probe(dev.as_ref()), Some(FsType::Fat12 | FsType::Fat16 | FsType::Fat32))
}

fn is_rpi_boot(fs: &mut dyn FileSystem) -> bool {
    if !exists(fs, "/config.txt") {
        return false;
    }
    if exists(fs, "/cmdline.txt") || exists(fs, "/bootcode.bin") {
        return true;
    }
    let root = fs.root_ino();
    fs.readdir(root).map(|v| v.iter().any(|d| {
        let n = d.name.to_ascii_lowercase();
        n.starts_with("start") && n.ends_with(".elf")
    })).unwrap_or(false)
}

/// The boot partition (and the root one), opened as partition views of `dev`.
struct Located {
    info: RpiInfo,
    boot: Arc<dyn BlockDev>,
    /// Files present on the boot partition we care about (lower case).
    cloud_files: Vec<&'static str>,
    has_root_cmdline: bool,
}

fn locate(dev: Arc<dyn BlockDev>) -> ForgeResult<Option<Located>> {
    let parts = block::parse_partitions(dev.as_ref())?;
    let mut views: Vec<(u32, Arc<dyn BlockDev>)> = vec![];
    for p in parts.iter().take(MAX_PARTITIONS) {
        if let Ok(v) = PartitionDev::new(dev.clone(), p.start, p.size) {
            views.push((p.index, Arc::new(v)));
        }
    }
    let mut found = None;
    for (idx, v) in &views {
        if !fat_of(v) {
            continue;
        }
        let Ok(mut fs) = vfs::open_fs(v.clone(), true, MountDefaults::default()) else { continue };
        if is_rpi_boot(fs.as_mut()) {
            found = Some((*idx, v.clone(), fs));
            break;
        }
    }
    let Some((boot_idx, boot, mut bfs)) = found else { return Ok(None) };
    let issue = read_text(bfs.as_mut(), "/issue.txt")?.unwrap_or_default();
    let cmdline = read_text(bfs.as_mut(), "/cmdline.txt")?;
    let mut cloud_files = vec![];
    for f in ["user-data", "meta-data", "network-config"] {
        if exists(bfs.as_mut(), &format!("/{f}")) {
            cloud_files.push(f);
        }
    }
    drop(bfs);

    // Root: the partition cmdline.txt names (root=PARTUUID=xxxxxxxx-NN), else the first ext one.
    let wanted: Option<u32> = cmdline.as_deref().and_then(|c| {
        c.split_whitespace().find_map(|w| w.strip_prefix("root=PARTUUID=")).and_then(|u| u.rsplit_once('-')).and_then(|(_, n)| u32::from_str_radix(n, 16).ok())
    });
    let mut order: Vec<&(u32, Arc<dyn BlockDev>)> = views.iter().filter(|(i, _)| *i != boot_idx).collect();
    order.sort_by_key(|(i, _)| Some(*i) != wanted);
    let mut root_idx = None;
    let mut rel: Vec<(String, String)> = vec![];
    let mut rpi_issue = false;
    for (idx, v) in order {
        if !matches!(vfs::probe(v.as_ref()), Some(FsType::Ext2 | FsType::Ext3 | FsType::Ext4)) {
            continue;
        }
        let Ok(mut fs) = vfs::open_fs(v.clone(), true, MountDefaults::default()) else { continue };
        root_idx = Some(*idx);
        let text = match read_text(fs.as_mut(), "/etc/os-release")? {
            Some(t) => Some(t),
            None => read_text(fs.as_mut(), "/usr/lib/os-release")?,
        };
        rel = text.map(|t| os_release(&t)).unwrap_or_default();
        rpi_issue = exists(fs.as_mut(), "/etc/rpi-issue");
        break;
    }

    let codename = get(&rel, "VERSION_CODENAME").map(String::from);
    let raspios = issue.starts_with("Raspberry Pi reference") || rpi_issue;
    let ubuntu = get(&rel, "ID") == Some("ubuntu");
    let (family, os) = if raspios {
        ("raspios", match &codename {
            Some(c) => format!("Raspberry Pi OS ({c})"),
            None => "Raspberry Pi OS".to_string(),
        })
    } else if ubuntu {
        ("ubuntu", format!("{} (Raspberry Pi)", get(&rel, "PRETTY_NAME").unwrap_or("Ubuntu")))
    } else {
        ("other", get(&rel, "PRETTY_NAME").map(|p| format!("{p} (Raspberry Pi)")).unwrap_or_else(|| "Raspberry Pi boot partition".into()))
    };
    let method = if !cloud_files.is_empty() {
        "cloud-init"
    } else if raspios {
        "firstrun"
    } else {
        "none"
    };
    Ok(Some(Located {
        info: RpiInfo { os, method: method.into(), family: family.into(), codename, boot_partition: boot_idx, root_partition: root_idx },
        boot,
        cloud_files,
        has_root_cmdline: cmdline.is_some(),
    }))
}

/// Find a Raspberry Pi boot partition on an image or a written disk and identify the OS and
/// how it is customised. Ok(None): not a Raspberry Pi image.
pub fn detect(dev: Arc<dyn BlockDev>) -> ForgeResult<Option<RpiInfo>> {
    Ok(locate(dev)?.map(|l| l.info))
}

// ───────────────────────────── validation ─────────────────────────────

/// Validated options: everything here is safe to quote into shell / YAML.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Settings {
    pub hostname: Option<String>,
    pub user: Option<String>,
    /// Plain password (CR/LF removed). Never logged.
    pub password: Option<String>,
    pub ssh: bool,
    pub keys: Vec<String>,
    pub wifi: Option<WifiSettings>,
    pub locale: Option<String>,
    pub time_zone: Option<String>,
    pub keyboard: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WifiSettings {
    pub ssid: String,
    /// Empty: open network. Never logged.
    pub password: String,
    /// Upper case ISO 3166-1 alpha-2.
    pub country: String,
    pub hidden: bool,
}

impl Settings {
    pub fn is_empty(&self) -> bool {
        self.hostname.is_none() && self.user.is_none() && self.password.is_none() && !self.ssh && self.wifi.is_none() && self.locale.is_none() && self.time_zone.is_none() && self.keyboard.is_none()
    }
}

fn invalid(it: &str, en: &str) -> ForgeError {
    ForgeError::Invalid(t(it, en))
}

fn nonempty(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// RFC 1123 host name label: 1–63 letters, digits and hyphens, not starting or ending with a
/// hyphen. (A single label: /etc/hostname, raspi-config and cloud-init `hostname` take one.)
pub fn valid_hostname(h: &str) -> bool {
    !h.is_empty() && h.len() <= 63 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !h.starts_with('-') && !h.ends_with('-')
}

const RESERVED_USERS: &[&str] = &[
    "root", "daemon", "bin", "sys", "sync", "games", "man", "lp", "mail", "news", "uucp", "proxy", "www-data", "backup", "list", "irc", "gnats", "nobody", "messagebus", "sshd", "avahi", "polkitd", "dnsmasq", "statd", "lightdm", "rtkit", "pulse", "colord", "saned", "nm-openvpn", "tss", "landscape", "pollinate", "syslog", "uuidd", "tcpdump", "usbmux", "dhcpcd", "rpi-first-boot-wizard",
];

/// Debian user name rules (adduser's NAME_REGEX: lower case letter first, then lower case
/// letters, digits, `-`; at most 32), restricted to what Raspberry Pi OS's `userconf` accepts
/// (no `_`), and not a system account.
pub fn valid_username(u: &str) -> bool {
    let b = u.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0].is_ascii_lowercase() && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-') && !RESERVED_USERS.contains(&u) && !u.starts_with("systemd-")
}

const KEY_TYPES: &[&str] = &[
    "ssh-ed25519", "ssh-rsa", "ssh-dss", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521", "sk-ssh-ed25519@openssh.com", "sk-ecdsa-sha2-nistp256@openssh.com", "ssh-ed448",
];

/// One `authorized_keys` line: optional options, a key type, base64 key, optional comment.
pub fn valid_ssh_key(line: &str) -> bool {
    if line.len() > 16 << 10 || line.chars().any(|c| c.is_control()) {
        return false;
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    words.windows(2).any(|w| KEY_TYPES.contains(&w[0]) && w[1].len() >= 16 && w[1].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='))
}

/// WPA passphrase (8–63 printable ASCII) or raw PSK (64 hex digits).
pub fn valid_wpa_key(p: &str) -> bool {
    is_hex_psk(p) || (8..=63).contains(&p.len()) && p.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

fn is_hex_psk(p: &str) -> bool {
    p.len() == 64 && p.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn valid_locale(l: &str) -> bool {
    // ll[_CC][.charset][@modifier], or C / POSIX.
    let (main, modifier) = l.split_once('@').map(|(a, b)| (a, Some(b))).unwrap_or((l, None));
    let (lang, charset) = main.split_once('.').map(|(a, b)| (a, Some(b))).unwrap_or((main, None));
    let (ll, cc) = lang.split_once('_').map(|(a, b)| (a, Some(b))).unwrap_or((lang, None));
    let alnum = |s: &str, min: usize, max: usize, extra: &[u8]| (min..=max).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || extra.contains(&b));
    l.len() <= 64
        && (alnum(ll, 2, 3, b"") && ll.bytes().all(|b| b.is_ascii_lowercase()) || ll == "C" || ll == "POSIX")
        && cc.map_or(true, |c| alnum(c, 2, 3, b""))
        && charset.map_or(true, |c| alnum(c, 1, 16, b"-"))
        && modifier.map_or(true, |m| alnum(m, 1, 16, b""))
}

pub fn valid_time_zone(z: &str) -> bool {
    z.len() <= 64 && !z.is_empty() && z.split('/').all(|c| !c.is_empty() && c != "." && c != ".." && !c.starts_with('-') && c.bytes().all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b)))
}

pub fn valid_keyboard(k: &str) -> bool {
    let b = k.as_bytes();
    !b.is_empty() && b.len() <= 32 && b[0].is_ascii_lowercase() && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}

/// Check every option; Err(Invalid) with a message naming the field, before anything is written.
pub fn validate(o: &RpiOptions) -> ForgeResult<Settings> {
    let mut s = Settings::default();
    if let Some(h) = nonempty(&o.hostname) {
        if !valid_hostname(&h) {
            return Err(invalid(
                "nome host non valido: 1–63 lettere, cifre o trattini, senza trattino iniziale o finale",
                "invalid host name: 1–63 letters, digits or hyphens, not starting or ending with a hyphen",
            ));
        }
        s.hostname = Some(h);
    }
    if let Some(u) = nonempty(&o.user) {
        if !valid_username(&u) {
            return Err(invalid(
                "nome utente non valido: minuscole, cifre e trattini, inizia con una lettera, massimo 32 caratteri, non un account di sistema",
                "invalid user name: lower case letters, digits and hyphens, starting with a letter, at most 32 characters, not a system account",
            ));
        }
        s.user = Some(u);
    }
    if let Some(p) = o.password.as_deref() {
        // Pasted text can carry a trailing newline, which PAM never sees (as Imager does).
        let p: String = p.chars().filter(|&c| c != '\r' && c != '\n').collect();
        if !p.is_empty() {
            if p.len() > crypt::MAX_PASSWORD || p.chars().any(|c| c.is_control()) {
                return Err(invalid("password non valida (caratteri di controllo o troppo lunga)", "invalid password (control characters or too long)"));
            }
            if s.user.is_none() {
                return Err(invalid("una password richiede un nome utente", "a password needs a user name"));
            }
            s.password = Some(p);
        }
    }
    s.ssh = o.ssh;
    if let Some(keys) = o.ssh_authorized_keys.as_deref() {
        for line in keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if !valid_ssh_key(line) {
                return Err(invalid("chiave SSH pubblica non valida (una per riga, es. «ssh-ed25519 AAAA… commento»)", "invalid SSH public key (one per line, e.g. \"ssh-ed25519 AAAA… comment\")"));
            }
            s.keys.push(line.to_string());
        }
        if s.keys.len() > 100 {
            return Err(invalid("troppe chiavi SSH (massimo 100)", "too many SSH keys (at most 100)"));
        }
    }
    if s.user.is_some() && s.password.is_none() && !(s.ssh && !s.keys.is_empty()) {
        return Err(invalid(
            "l'account non avrebbe modo di accedere: indica una password o una chiave SSH",
            "the account would have no way to sign in: give a password or an SSH key",
        ));
    }
    if s.ssh && s.keys.is_empty() && s.password.is_none() {
        return Err(invalid("SSH con password richiede utente e password", "SSH with password login needs a user and a password"));
    }
    if let Some(w) = &o.wifi {
        let ssid = w.ssid.clone();
        if ssid.is_empty() || ssid.len() > 32 || ssid.chars().any(|c| c.is_control()) {
            return Err(invalid("SSID Wi-Fi non valido: da 1 a 32 byte, senza caratteri di controllo", "invalid Wi-Fi SSID: 1 to 32 bytes, no control characters"));
        }
        let password: String = w.password.chars().filter(|&c| c != '\r' && c != '\n').collect();
        if !password.is_empty() && !valid_wpa_key(&password) {
            return Err(invalid(
                "password Wi-Fi non valida: da 8 a 63 caratteri ASCII stampabili, oppure 64 cifre esadecimali",
                "invalid Wi-Fi password: 8 to 63 printable ASCII characters, or 64 hex digits",
            ));
        }
        let country = w.country.trim().to_ascii_uppercase();
        if country.len() != 2 || !country.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(invalid(
                "il paese del Wi-Fi è obbligatorio (codice ISO 3166 di due lettere, es. IT): stabilisce le frequenze consentite",
                "the Wi-Fi country is required (two-letter ISO 3166 code, e.g. IT): it sets the allowed frequencies",
            ));
        }
        s.wifi = Some(WifiSettings { ssid, password, country, hidden: w.hidden });
    }
    if let Some(l) = nonempty(&o.locale) {
        if !valid_locale(&l) {
            return Err(invalid("lingua (locale) non valida, es. it_IT.UTF-8", "invalid locale, e.g. en_GB.UTF-8"));
        }
        s.locale = Some(l);
    }
    if let Some(z) = nonempty(&o.time_zone) {
        if !valid_time_zone(&z) {
            return Err(invalid("fuso orario non valido, es. Europe/Rome", "invalid time zone, e.g. Europe/London"));
        }
        s.time_zone = Some(z);
    }
    if let Some(k) = nonempty(&o.keyboard_layout) {
        if !valid_keyboard(&k) {
            return Err(invalid("layout di tastiera non valido, es. it", "invalid keyboard layout, e.g. gb"));
        }
        s.keyboard = Some(k);
    }
    Ok(s)
}

// ───────────────────────────── quoting ─────────────────────────────

/// POSIX shell single-quoted word.
pub fn sh_quote(v: &str) -> String {
    format!("'{}'", v.replace('\'', "'\"'\"'"))
}

/// YAML double-quoted scalar (printable UTF-8 kept, the rest escaped).
pub fn yaml_quote(v: &str) -> String {
    let mut o = String::with_capacity(v.len() + 2);
    o.push('"');
    for c in v.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\0' => o.push_str("\\0"),
            c if (c as u32) < 0x20 || c == '\u{7f}' || ((c as u32) >= 0x80 && (c as u32) < 0xA0) => o.push_str(&format!("\\x{:02x}", c as u32)),
            '\u{2028}' | '\u{2029}' | '\u{FEFF}' | '\u{FFFE}' | '\u{FFFF}' => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A here-document delimiter no line of `body` equals.
fn heredoc_delim(body: &str, base: &str) -> String {
    let mut cand = base.to_string();
    let mut n = 0;
    while body.lines().any(|l| l == cand) {
        n += 1;
        cand = format!("{base}{n}");
    }
    cand
}

/// GLib key-file escaping of a value (NetworkManager unescapes it), as Imager does.
fn keyfile_escape(v: &str) -> String {
    let mut o = String::new();
    for (i, c) in v.chars().enumerate() {
        match c {
            '\\' => o.push_str("\\\\"),
            ' ' if i == 0 => o.push_str("\\s"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o
}

// ───────────────────────────── firstrun ─────────────────────────────

const IMAGER_CUSTOM: &str = "/usr/lib/raspberrypi-sys-mods/imager_custom";

/// The user an operation applies to when none was named (Imager: "pi").
fn effective_user(s: &Settings) -> &str {
    s.user.as_deref().unwrap_or("pi")
}

/// `firstrun.sh`: Raspberry Pi Imager 2.0.12's `generateSystemdScript`, plus the locale and the
/// removal of `userconf.txt`. `hash` is the crypted password.
pub fn firstrun_script(s: &Settings, hash: Option<&str>, wrote_userconf: bool) -> String {
    let mut o = String::new();
    let mut l = |line: &str| {
        o.push_str(line);
        o.push('\n');
    };
    l("#!/bin/sh");
    l("");
    l("set +e");
    l("");
    if let Some(h) = &s.hostname {
        l(&format!("IMAGER_HOSTNAME={}", sh_quote(h)));
        l("CURRENT_HOSTNAME=$(cat /etc/hostname | tr -d \" \\t\\n\\r\")");
        l(&format!("if [ -f {IMAGER_CUSTOM} ]; then"));
        l(&format!("   {IMAGER_CUSTOM} set_hostname \"$IMAGER_HOSTNAME\""));
        l("else");
        l("   echo \"$IMAGER_HOSTNAME\" >/etc/hostname");
        l("   IMAGER_HOSTNAME_SED=$(printf '%s' \"$IMAGER_HOSTNAME\" | sed -e 's/[\\\\/&]/\\\\&/g')");
        l("   sed -i \"s/127.0.1.1.*$CURRENT_HOSTNAME/127.0.1.1\\t$IMAGER_HOSTNAME_SED/g\" /etc/hosts");
        l("fi");
    }
    l("FIRSTUSER=$(getent passwd 1000 | cut -d: -f1)");
    l("FIRSTUSERHOME=$(getent passwd 1000 | cut -d: -f6)");
    let keys: &[String] = if s.ssh { &s.keys } else { &[] };
    if !keys.is_empty() {
        let args: String = keys.iter().map(|k| format!(" {}", sh_quote(k))).collect();
        l(&format!("if [ -f {IMAGER_CUSTOM} ]; then"));
        l(&format!("   {IMAGER_CUSTOM} enable_ssh -k{args}"));
        l("else");
        l("   install -o \"$FIRSTUSER\" -m 700 -d \"$FIRSTUSERHOME/.ssh\"");
        let all = keys.join("\n");
        let d = heredoc_delim(&all, "EOF");
        l(&format!("cat > \"$FIRSTUSERHOME/.ssh/authorized_keys\" <<'{d}'"));
        l(&all);
        l(&d);
        l("   chown \"$FIRSTUSER:$FIRSTUSER\" \"$FIRSTUSERHOME/.ssh/authorized_keys\"");
        l("   chmod 600 \"$FIRSTUSERHOME/.ssh/authorized_keys\"");
        l("   echo 'PasswordAuthentication no' >>/etc/ssh/sshd_config");
        l("   systemctl enable ssh");
        l("fi");
    }
    if s.ssh && keys.is_empty() {
        l(&format!("if [ -f {IMAGER_CUSTOM} ]; then"));
        l(&format!("   {IMAGER_CUSTOM} enable_ssh"));
        l("else");
        l("   systemctl enable ssh");
        l("fi");
    }
    if s.user.is_some() || hash.is_some() || !keys.is_empty() {
        let pass = hash.unwrap_or("");
        l(&format!("IMAGER_USER={}", sh_quote(effective_user(s))));
        if hash.is_some() {
            l(&format!("IMAGER_PASS={}", sh_quote(pass)));
        }
        l("IMAGER_USER_SED=$(printf '%s' \"$IMAGER_USER\" | sed -e 's/[\\\\/&]/\\\\&/g')");
        l("if [ -f /usr/lib/userconf-pi/userconf ]; then");
        l(&format!("   /usr/lib/userconf-pi/userconf \"$IMAGER_USER\" {}", sh_quote(pass)));
        l("else");
        if hash.is_some() {
            l("   echo \"$FIRSTUSER:$IMAGER_PASS\" | chpasswd -e");
        }
        l("   if [ \"$FIRSTUSER\" != \"$IMAGER_USER\" ]; then");
        l("      usermod -l \"$IMAGER_USER\" \"$FIRSTUSER\"");
        l("      usermod -m -d \"/home/$IMAGER_USER\" \"$IMAGER_USER\"");
        l("      groupmod -n \"$IMAGER_USER\" \"$FIRSTUSER\"");
        l("      if grep -q \"^autologin-user=\" /etc/lightdm/lightdm.conf ; then");
        l("         sed /etc/lightdm/lightdm.conf -i -e \"s/^autologin-user=.*/autologin-user=$IMAGER_USER_SED/\"");
        l("      fi");
        l("      if [ -f /etc/systemd/system/getty@tty1.service.d/autologin.conf ]; then");
        l("         sed /etc/systemd/system/getty@tty1.service.d/autologin.conf -i -e \"s/$FIRSTUSER/$IMAGER_USER_SED/\"");
        l("      fi");
        l("      if [ -f /etc/sudoers.d/010_pi-nopasswd ]; then");
        l("         sed -i \"s/^$FIRSTUSER /$IMAGER_USER_SED /\" /etc/sudoers.d/010_pi-nopasswd");
        l("      fi");
        l("   fi");
        l("fi");
    }
    if let Some(w) = &s.wifi {
        l(&format!("if [ -f {IMAGER_CUSTOM} ] && true; then"));
        let mut pass_arg = sh_quote(&w.password);
        let escaped = keyfile_escape(&w.password);
        if escaped != w.password {
            // imager_custom since raspberrypi-sys-mods c8a2ee70 pastes PASS into the keyfile
            // unescaped; the raspi-config path before it needs it raw.
            l(&format!("   IMAGER_WLAN_PASS={pass_arg}"));
            l(&format!("   if grep -qF {} {IMAGER_CUSTOM}; then IMAGER_WLAN_PASS={}; fi", sh_quote("psk=${PASS}"), sh_quote(&escaped)));
            pass_arg = "\"$IMAGER_WLAN_PASS\"".into();
        }
        let mut cmd = format!("   {IMAGER_CUSTOM} set_wlan");
        if w.hidden {
            cmd.push_str(" -h");
        }
        // A raw PSK goes unflagged so raspi-config hands it to wpa_cli as hex.
        if !w.password.is_empty() && !is_hex_psk(&w.password) {
            cmd.push_str(" -p");
        }
        cmd.push_str(&format!(" {} {} {}", sh_quote(&w.ssid), pass_arg, sh_quote(&w.country)));
        l(&cmd);
        l("else");
        let mut body = format!("country={}\nctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\nap_scan=1\n\nupdate_config=1\nnetwork={{\n", w.country);
        if w.hidden {
            body.push_str("\tscan_ssid=1\n");
        }
        if w.ssid.contains('"') || w.ssid.contains('\\') {
            body.push_str(&format!("\tssid=hex:{}\n", w.ssid.bytes().map(|b| format!("{b:02x}")).collect::<String>()));
        } else {
            body.push_str(&format!("\tssid=\"{}\"\n", w.ssid));
        }
        if w.password.is_empty() {
            body.push_str("\tkey_mgmt=NONE\n");
        } else {
            body.push_str("\tkey_mgmt=WPA-PSK SAE\n");
            if is_hex_psk(&w.password) {
                body.push_str(&format!("\tpsk={}\n", w.password));
            } else {
                // A quoted passphrase runs to the last '"', so inner quotes are safe.
                body.push_str(&format!("\tpsk=\"{}\"\n", w.password));
            }
            body.push_str("\tieee80211w=1\n");
        }
        body.push_str("}\n");
        let d = heredoc_delim(&body, "WPAEOF");
        l(&format!("cat >/etc/wpa_supplicant/wpa_supplicant.conf <<'{d}'"));
        o_push_raw(&mut l, &body);
        l(&d);
        l("   chmod 600 /etc/wpa_supplicant/wpa_supplicant.conf");
        l("   rfkill unblock wifi");
        l("   for filename in /var/lib/systemd/rfkill/*:wlan ; do");
        l("       echo 0 > $filename");
        l("   done");
        l("fi");
    }
    if s.keyboard.is_some() || s.time_zone.is_some() {
        l(&format!("if [ -f {IMAGER_CUSTOM} ]; then"));
        if let Some(k) = &s.keyboard {
            l(&format!("   {IMAGER_CUSTOM} set_keymap {}", sh_quote(k)));
        }
        if let Some(z) = &s.time_zone {
            l(&format!("   {IMAGER_CUSTOM} set_timezone {}", sh_quote(z)));
        }
        l("else");
        if let Some(z) = &s.time_zone {
            l("   rm -f /etc/localtime");
            l(&format!("   echo {} >/etc/timezone", sh_quote(z)));
            l("   dpkg-reconfigure -f noninteractive tzdata");
        }
        if let Some(k) = &s.keyboard {
            let d = heredoc_delim(k, "KBEOF");
            l(&format!("cat >/etc/default/keyboard <<'{d}'"));
            l("XKBMODEL=\"pc105\"");
            l(&format!("XKBLAYOUT=\"{k}\""));
            l("XKBVARIANT=\"\"");
            l("XKBOPTIONS=\"\"");
            l("");
            l(&d);
            l("   dpkg-reconfigure -f noninteractive keyboard-configuration");
        }
        l("fi");
    }
    if let Some(loc) = &s.locale {
        // Not in Imager (it has no locale option for this format): raspi-config's own helper.
        l(&format!("IMAGER_LOCALE={}", sh_quote(loc)));
        l("if command -v raspi-config >/dev/null 2>&1; then");
        l("   raspi-config nonint do_change_locale \"$IMAGER_LOCALE\"");
        l("else");
        l("   sed -i \"s/^# *\\($IMAGER_LOCALE\\)/\\1/\" /etc/locale.gen");
        l("   locale-gen");
        l("   update-locale LANG=\"$IMAGER_LOCALE\"");
        l("fi");
    }
    if wrote_userconf {
        // The account is set: the fallback file (and its hash) goes. Before the line below,
        // which bookworm's initramfs (imager_fixup) cuts the script at.
        l("rm -f /boot/userconf.txt /boot/firmware/userconf.txt");
    }
    l("rm -f /boot/firstrun.sh");
    l("sed -i 's| systemd.run.*||g' /boot/cmdline.txt");
    l("exit 0");
    o
}

/// Append a multi-line body (each line already terminated) through the line writer.
fn o_push_raw(l: &mut impl FnMut(&str), body: &str) {
    for line in body.strip_suffix('\n').unwrap_or(body).split('\n') {
        l(line);
    }
}

/// Remove parameters LumenDisk may have added before (so customising twice doesn't stack them).
fn strip_ours(cmdline: &str) -> String {
    let mut out: Vec<&str> = vec![];
    let mut words = cmdline.split_whitespace().peekable();
    while let Some(w) = words.next() {
        if w.starts_with("systemd.run=") || w.starts_with("systemd.run_success_action=") || w == "systemd.unit=kernel-command-line.target" || w.starts_with("cfg80211.ieee80211_regdom=") || w.starts_with("ds=nocloud") {
            continue;
        }
        out.push(w);
    }
    out.join(" ")
}

/// `cmdline.txt` for the firstrun method: one line, + regdom + the systemd.run parameters.
pub fn cmdline_firstrun(orig: &str, country: Option<&str>) -> String {
    let mut c = strip_ours(orig);
    if let Some(cc) = country {
        c.push_str(&format!(" cfg80211.ieee80211_regdom={cc}"));
    }
    c.push_str(" systemd.run=/boot/firstrun.sh systemd.run_success_action=reboot systemd.unit=kernel-command-line.target");
    c.push('\n');
    c
}

/// `cmdline.txt` for the cloud-init method on Raspberry Pi OS: + regdom + `ds=nocloud;i=<id>`.
pub fn cmdline_cloud(orig: &str, country: Option<&str>, instance_id: &str) -> String {
    let mut c = strip_ours(orig);
    if let Some(cc) = country {
        c.push_str(&format!(" cfg80211.ieee80211_regdom={cc}"));
    }
    c.push_str(&format!(" ds=nocloud;i={instance_id}"));
    c.push('\n');
    c
}

pub fn userconf_txt(user: &str, hash: &str) -> String {
    format!("{user}:{hash}\n")
}

// ───────────────────────────── cloud-init ─────────────────────────────

/// One top-level block of a block-style YAML mapping.
#[derive(Debug)]
struct Block {
    key: Option<String>,
    /// Comment / blank lines before the key line (kept even when the block is replaced).
    lead: Vec<String>,
    /// The key line and its continuation.
    lines: Vec<String>,
}

/// Split a `#cloud-config` document into top-level blocks (comments before a key belong to it).
/// None when the document isn't plain block-style YAML we can edit safely.
fn split_blocks(text: &str) -> Option<Vec<Block>> {
    let mut blocks: Vec<Block> = vec![];
    let mut pending: Vec<String> = vec![];
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        if n == 0 && line.trim_start_matches('\u{feff}').starts_with("#cloud-config") {
            continue;
        }
        let first = line.chars().next();
        match first {
            None | Some('#') => pending.push(line.into()),
            _ if line.trim().is_empty() => pending.push(line.into()),
            Some(' ') | Some('\t') | Some('-') => {
                if line == "---" && blocks.is_empty() {
                    continue;
                }
                let b = blocks.last_mut()?;
                b.lines.append(&mut pending);
                b.lines.push(line.into());
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let (k, rest) = line.split_once(':')?;
                if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
                    return None;
                }
                let lead = std::mem::take(&mut pending);
                blocks.push(Block { key: Some(k.to_string()), lead, lines: vec![line.into()] });
            }
            _ => return None,
        }
    }
    if !pending.is_empty() {
        blocks.push(Block { key: None, lead: pending, lines: vec![] });
    }
    Some(blocks)
}

/// Is the value of this block a block-style sequence ("key:" then "- item" lines)?
fn seq_indent(b: &Block) -> Option<usize> {
    let mut seen_key = false;
    for l in &b.lines {
        if !seen_key {
            if l.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                let rest = l.split_once(':').map(|x| x.1).unwrap_or("");
                if !rest.split('#').next().unwrap_or("").trim().is_empty() {
                    return None; // flow style / scalar on the key line
                }
                seen_key = true;
            }
            continue;
        }
        let t = l.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        return t.starts_with("- ").then(|| l.len() - t.len());
    }
    None
}

/// The pieces of user-data LumenDisk generates: (key, value lines after "key:" or the
/// whole block for scalars, list items for sequences).
enum Gen {
    Block(&'static str, Vec<String>),
    Seq(&'static str, Vec<String>),
}

fn user_data_blocks(s: &Settings, hash: Option<&str>, family: &str) -> Vec<Gen> {
    let mut g = vec![Gen::Block("manage_resolv_conf", vec!["manage_resolv_conf: false".into()])];
    if let Some(h) = &s.hostname {
        g.push(Gen::Block("hostname", vec![format!("hostname: {}", yaml_quote(h))]));
        g.push(Gen::Block("manage_etc_hosts", vec!["manage_etc_hosts: true".into()]));
        // Imager parity: mDNS (`<host>.local`) and no apt date check on a clock not yet set.
        g.push(Gen::Seq("packages", vec!["avahi-daemon".into()]));
        g.push(Gen::Block(
            "apt",
            vec!["apt:".into(), "  preserve_sources_list: true".into(), "  conf: |".into(), "    Acquire {".into(), "      Check-Date \"false\";".into(), "    };".into()],
        ));
    }
    if let Some(z) = &s.time_zone {
        g.push(Gen::Block("timezone", vec![format!("timezone: {}", yaml_quote(z))]));
    }
    if let Some(k) = &s.keyboard {
        g.push(Gen::Block("keyboard", vec!["keyboard:".into(), "  model: pc105".into(), format!("  layout: {}", yaml_quote(k))]));
    }
    if let Some(loc) = &s.locale {
        g.push(Gen::Block("locale", vec![format!("locale: {}", yaml_quote(loc))]));
    }
    let keys: &[String] = if s.ssh { &s.keys } else { &[] };
    if hash.is_some() || !keys.is_empty() {
        let default = if family == "ubuntu" { "ubuntu" } else { "pi" };
        let name = s.user.as_deref().unwrap_or(default);
        // Singular `user:` renames / extends the distro's default user and keeps its groups
        // (sudo, adm, gpio…); `sudo: null` drops the default NOPASSWD rule (Imager 2.0.12).
        let mut u = vec!["user:".into(), format!("  name: {}", yaml_quote(name)), "  shell: /bin/bash".into()];
        if let Some(h) = hash {
            u.push("  lock_passwd: false".into());
            u.push(format!("  passwd: {}", yaml_quote(h)));
        } else {
            u.push("  lock_passwd: true".into());
        }
        if !keys.is_empty() {
            u.push("  ssh_authorized_keys:".into());
            for k in keys {
                u.push(format!("    - {}", yaml_quote(k)));
            }
        }
        u.push("  sudo: null".into());
        g.push(Gen::Block("user", u));
    }
    if s.ssh {
        g.push(Gen::Block("ssh_pwauth", vec![format!("ssh_pwauth: {}", keys.is_empty())]));
        // cc_ssh never enables sshd; Raspberry Pi OS ships it disabled.
        g.push(Gen::Seq("runcmd", vec!["[ systemctl, enable, --now, ssh ]".into()]));
    }
    g
}

/// `user-data`: the generated settings merged into `existing` (see the module docs). Returns
/// the document and notes on what was replaced.
pub fn user_data(s: &Settings, hash: Option<&str>, family: &str, existing: Option<&str>) -> (String, Vec<String>) {
    let gen = user_data_blocks(s, hash, family);
    let mut notes = vec![];
    let mut kept: Vec<Block> = match existing {
        None => vec![],
        Some(text) => {
            let is_cc = text.trim_start_matches('\u{feff}').starts_with("#cloud-config");
            let all_comments = text.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#'));
            match split_blocks(text) {
                Some(b) if is_cc || all_comments => b,
                _ => {
                    notes.push("user-data: not a block-style #cloud-config document, replaced".into());
                    vec![]
                }
            }
        }
    };
    let ours: Vec<&str> = gen.iter().map(|g| match g {
        Gen::Block(k, _) | Gen::Seq(k, _) => *k,
    }).collect();
    let user_set = ours.contains(&"user");
    let mut out = String::from("#cloud-config\n");
    // Existing sequences we extend: (key, indent, existing block).
    let mut extend: Vec<(&'static str, usize, Block)> = vec![];
    let mut rest: Vec<Block> = vec![];
    for b in kept.drain(..) {
        let Some(k) = b.key.clone() else {
            rest.push(b);
            continue;
        };
        if user_set && (k == "chpasswd" || k == "password") {
            notes.push(format!("user-data: removed `{k}` (conflicts with the configured account)"));
            rest.push(Block { key: None, lead: b.lead, lines: vec![] });
            continue;
        }
        if let Some(g) = gen.iter().find(|g| matches!(g, Gen::Block(gk, _) | Gen::Seq(gk, _) if *gk == k)) {
            match (g, seq_indent(&b)) {
                (Gen::Seq(gk, _), Some(ind)) => extend.push((gk, ind, b)),
                _ => {
                    notes.push(format!("user-data: replaced `{k}`"));
                    rest.push(Block { key: None, lead: b.lead, lines: vec![] });
                }
            }
            continue;
        }
        rest.push(b);
    }
    for b in &rest {
        for l in b.lead.iter().chain(&b.lines) {
            out.push_str(l);
            out.push('\n');
        }
    }
    if !rest.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&format!("# Set by LumenDisk (Raspberry Pi Imager {IMAGER_VERSION} conventions)\n"));
    for g in &gen {
        match g {
            Gen::Block(_, lines) => {
                for l in lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            Gen::Seq(k, items) => {
                if let Some(pos) = extend.iter().position(|(ek, _, _)| ek == k) {
                    let (_, ind, b) = extend.remove(pos);
                    let have: Vec<String> = b.lines.iter().map(|l| l.trim().to_string()).collect();
                    for l in b.lead.iter().chain(&b.lines) {
                        out.push_str(l);
                        out.push('\n');
                    }
                    for it in items {
                        if !have.contains(&format!("- {it}")) {
                            out.push_str(&format!("{}- {it}\n", " ".repeat(ind)));
                        }
                    }
                } else {
                    out.push_str(&format!("{k}:\n"));
                    for it in items {
                        out.push_str(&format!("  - {it}\n"));
                    }
                }
            }
        }
    }
    (out, notes)
}

/// `network-config` (netplan v2) for Wi-Fi: eth0 keeps DHCP, wlan0 joins the network.
pub fn network_config(w: &WifiSettings) -> String {
    let mut o = String::from("# Set by LumenDisk (Raspberry Pi Imager conventions): netplan v2, applied by cloud-init on first boot.\n");
    o.push_str("network:\n  version: 2\n  ethernets:\n    eth0:\n      dhcp4: true\n      dhcp6: true\n      optional: true\n");
    o.push_str("  wifis:\n    wlan0:\n      dhcp4: true\n");
    o.push_str(&format!("      regulatory-domain: {}\n", yaml_quote(&w.country)));
    o.push_str("      access-points:\n");
    o.push_str(&format!("        {}:\n", yaml_quote(&w.ssid)));
    if w.hidden {
        o.push_str("          hidden: true\n");
    }
    if w.password.is_empty() {
        o.push_str("          auth:\n            key-management: none\n");
    } else {
        o.push_str(&format!("          password: {}\n", yaml_quote(&w.password)));
    }
    o.push_str("      optional: true\n");
    o
}

/// `meta-data`: every existing line but the instance id, then the new one.
pub fn meta_data(existing: Option<&str>, instance_id: &str) -> String {
    let mut o = String::new();
    let mut has_dsmode = false;
    for l in existing.unwrap_or("").lines() {
        let k = l.trim_start();
        if k.starts_with("instance-id:") || k.starts_with("instance_id:") {
            continue;
        }
        has_dsmode |= k.starts_with("dsmode:");
        o.push_str(l.trim_end_matches('\r'));
        o.push('\n');
    }
    if !has_dsmode && existing.is_none() {
        o.push_str("dsmode: local\n");
    }
    o.push_str(&format!("instance-id: {instance_id}\n"));
    o
}

// ───────────────────────────── applying ─────────────────────────────

/// Write a small file atomically-ish: a temporary name, then a rename over the old one (a
/// crash never leaves the boot partition without its cmdline.txt).
fn put(fs: &mut dyn FileSystem, name: &str, data: &[u8]) -> ForgeResult<()> {
    let tmp = "lumendisk.tmp";
    let root = fs.root_ino();
    match fs.unlink(root, tmp) {
        Ok(()) | Err(FsError::NotFound) => {}
        Err(e) => return Err(e.into()),
    }
    super::tree::write_file(fs, &format!("/{tmp}"), data)?;
    // An existing file keeps its on-disk name (case), so look it up case-insensitively first.
    let existing = fs.readdir(root)?.into_iter().find(|d| d.name.eq_ignore_ascii_case(name)).map(|d| d.name);
    let dest = existing.unwrap_or_else(|| name.to_string());
    fs.rename(root, tmp, root, &dest)?;
    Ok(())
}

/// Customise the Raspberry Pi image or disk `dev` (already written). Result: method and files
/// written; the passwords are not in it.
pub fn customize(dev: Arc<dyn BlockDev>, opts: &RpiOptions, ctx: &JobCtx) -> ForgeResult<Value> {
    ctx.phase_units(Phase::Customizing, 0, "files");
    let s = validate(opts)?;
    let loc = locate(dev.clone())?.ok_or_else(|| ForgeError::Unsupported(t("nessuna partizione di avvio Raspberry Pi trovata", "no Raspberry Pi boot partition found")))?;
    ctx.info(format!("{}: {} ({})", t("sistema", "system"), loc.info.os, loc.info.method));
    if s.is_empty() {
        ctx.info(t("nessuna personalizzazione richiesta", "no customisation requested"));
        return Ok(json!({"os": loc.info.os, "method": loc.info.method, "files": [], "notes": []}));
    }
    if loc.info.method == "none" {
        return Err(ForgeError::Unsupported(t(
            &format!("{}: personalizzazione al primo avvio non supportata (né cloud-init né firstrun)", loc.info.os),
            &format!("{}: first-boot customisation not supported (neither cloud-init nor firstrun)", loc.info.os),
        )));
    }
    if !s.ssh && !s.keys.is_empty() {
        ctx.warn(t("chiavi SSH ignorate: SSH non è attivato", "SSH keys ignored: SSH is not enabled"));
    }
    let hash = match &s.password {
        Some(p) => Some(crypt::hash(p).map_err(|e| ForgeError::Io(e.to_string()))?),
        None => None,
    };
    ctx.check()?;

    let mut fs = vfs::open_fs(loc.boot.clone(), false, MountDefaults::default())?;
    let mut files: Vec<(String, Vec<u8>)> = vec![];
    let mut notes: Vec<String> = vec![];
    let country = s.wifi.as_ref().map(|w| w.country.as_str());
    if loc.info.method == "firstrun" {
        let userconf = s.user.is_some() && hash.is_some();
        files.push(("firstrun.sh".into(), firstrun_script(&s, hash.as_deref(), userconf).into_bytes()));
        if let (true, Some(u), Some(h)) = (userconf, &s.user, &hash) {
            files.push(("userconf.txt".into(), userconf_txt(u, h).into_bytes()));
        }
        if s.ssh {
            files.push(("ssh".into(), vec![]));
        }
        let orig = read_text(fs.as_mut(), "/cmdline.txt")?.ok_or_else(|| ForgeError::Invalid(t("cmdline.txt mancante o illeggibile", "cmdline.txt missing or unreadable")))?;
        files.push(("cmdline.txt".into(), cmdline_firstrun(&orig, country).into_bytes()));
    } else {
        let mut id = [0u8; 8];
        getrandom::getrandom(&mut id).map_err(|e| ForgeError::Io(format!("random generator: {e}")))?;
        let instance_id = format!("lumendisk-{}", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let existing = read_text(fs.as_mut(), "/user-data")?;
        let (ud, mut n) = user_data(&s, hash.as_deref(), &loc.info.family, existing.as_deref());
        notes.append(&mut n);
        files.push(("user-data".into(), ud.into_bytes()));
        if let Some(w) = &s.wifi {
            if loc.cloud_files.contains(&"network-config") {
                notes.push("network-config: replaced (Wi-Fi)".into());
            }
            files.push(("network-config".into(), network_config(w).into_bytes()));
        }
        let md = read_text(fs.as_mut(), "/meta-data")?;
        files.push(("meta-data".into(), meta_data(md.as_deref(), &instance_id).into_bytes()));
        if loc.has_root_cmdline {
            let orig = read_text(fs.as_mut(), "/cmdline.txt")?.unwrap_or_default();
            files.push(("cmdline.txt".into(), cmdline_cloud(&orig, country, &instance_id).into_bytes()));
        } else {
            notes.push("cmdline.txt: none at the top of the boot partition, left as is".into());
        }
    }
    ctx.set_total(files.len() as u64);
    for (i, (name, data)) in files.iter().enumerate() {
        put(fs.as_mut(), name, data).map_err(|e| ForgeError::Io(format!("{name}: {e}")))?;
        ctx.set_done(i as u64 + 1);
    }
    fs.sync()?;
    drop(fs);
    dev.flush()?;
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    ctx.info(format!("{}: {}", t("file scritti", "files written"), names.join(", ")));
    for n in &notes {
        ctx.info(n.clone());
    }
    Ok(json!({
        "os": loc.info.os,
        "method": loc.info.method,
        "family": loc.info.family,
        "boot_partition": loc.info.boot_partition,
        "files": names,
        "notes": notes,
        "imager_version": IMAGER_VERSION,
    }))
}

#[cfg(test)]
mod tests;
