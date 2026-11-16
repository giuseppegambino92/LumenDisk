//! The requests the app sends (as JSON through the C ABI) and that the CLI accepts.
//! Each one is run as a job by [`run`]; engine modules implement the `run_*` functions.
//!
//! This file is the contract between the UI and the engine: field names are part of the ABI.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::job::JobCtx;
use super::target::TargetSpec;
use super::{ForgeResult, MkfsOptions, TargetFs};

/// Partition table scheme.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    Gpt,
    Mbr,
    Apm,
    /// No partition table: the filesystem starts at byte 0 ("superfloppy").
    None,
}

/// One partition to create.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartSpec {
    pub fs: TargetFs,
    #[serde(flatten)]
    pub mkfs: MkfsOptions,
    /// Size in bytes; None = the rest of the disk (only the last partition).
    #[serde(default)]
    pub size: Option<u64>,
    /// GPT partition name (defaults to the label).
    #[serde(default)]
    pub name: Option<String>,
}

/// Erase a disk and create partitions + filesystems.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FormatRequest {
    pub target: TargetSpec,
    pub scheme: Scheme,
    pub partitions: Vec<PartSpec>,
    /// Zero the first and last MiB and every partition's first MiB (old signatures), default true.
    #[serde(default = "yes")]
    pub clear_signatures: bool,
}

fn yes() -> bool {
    true
}

/// How an image is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteMode {
    /// Choose from the source: Windows → windows, hybrid Linux ISO → dd, other ISO → iso, image → dd.
    Auto,
    /// Byte-for-byte copy of the (decompressed) image.
    Dd,
    /// Extract the ISO's files onto a fresh filesystem and make it bootable (Rufus "ISO mode").
    Iso,
    /// Windows installation media.
    Windows,
}

/// Write an image (ISO, IMG, compressed, VHD, DMG) to a target.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlashRequest {
    pub source: String,
    pub target: TargetSpec,
    #[serde(default = "auto")]
    pub mode: WriteMode,
    /// Read the target back and compare after writing (default true).
    #[serde(default = "yes")]
    pub verify: bool,
    /// Expected checksum of the source ("sha256:…", "sha1:…", "md5:…"), checked before writing.
    #[serde(default)]
    pub expect_hash: Option<String>,
    #[serde(default)]
    pub windows: Option<WindowsOptions>,
    #[serde(default)]
    pub linux: Option<LinuxOptions>,
    #[serde(default)]
    pub rpi: Option<RpiOptions>,
    /// Eject the disk at the end (default true).
    #[serde(default = "yes")]
    pub eject: bool,
}

fn auto() -> WriteMode {
    WriteMode::Auto
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsLayout {
    /// FAT32, install.wim split into .swm when > 4 GiB. Boots UEFI and BIOS.
    Fat32Split,
    /// NTFS + a small UEFI:NTFS partition. Boots UEFI (incl. Secure Boot) and BIOS.
    NtfsUefi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootTarget {
    /// UEFI only (GPT).
    Uefi,
    /// BIOS / CSM (MBR, boot code installed).
    Bios,
    /// Both (MBR + EFI files): the default, works almost everywhere.
    Both,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WindowsOptions {
    #[serde(default)]
    pub layout: Option<WindowsLayout>,
    #[serde(default)]
    pub boot: Option<BootTarget>,
    #[serde(default)]
    pub label: Option<String>,
    /// Windows 11: skip the TPM 2.0 / Secure Boot / RAM / CPU / storage checks (LabConfig).
    #[serde(default)]
    pub bypass_hardware_checks: bool,
    /// Allow a local account during setup (no Microsoft account / network required, BypassNRO).
    #[serde(default)]
    pub local_account: Option<LocalAccount>,
    #[serde(default)]
    pub skip_privacy_questions: bool,
    /// Locale for the unattended file ("it-IT", "en-US"…); None = ask during setup.
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    /// A complete autounattend.xml supplied by the user, written as is (overrides the above).
    #[serde(default)]
    pub custom_answer_file: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalAccount {
    pub name: String,
    /// Empty = no password.
    #[serde(default)]
    pub password: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LinuxOptions {
    /// Size of the persistence partition in bytes (ISO mode); None/0 = no persistence.
    #[serde(default)]
    pub persistence_size: Option<u64>,
    /// Filesystem of the main partition in ISO mode (default FAT32).
    #[serde(default)]
    pub fs: Option<TargetFs>,
    #[serde(default)]
    pub boot: Option<BootTarget>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RpiOptions {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub ssh: bool,
    /// Public keys for SSH key login (one per line); empty = password login.
    #[serde(default)]
    pub ssh_authorized_keys: Option<String>,
    #[serde(default)]
    pub wifi: Option<Wifi>,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub keyboard_layout: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wifi {
    pub ssid: String,
    #[serde(default)]
    pub password: String,
    /// ISO 3166 country code ("IT"): required by the Wi-Fi regulatory domain.
    pub country: String,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WipeMethod {
    Zeros,
    Random,
    /// DoD 5220.22-M (3 passes: zeros, ones, random, then verification).
    Dod3,
    /// DoD 5220.22-M ECE (7 passes).
    Dod7,
    /// Gutmann (35 passes).
    Gutmann,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WipeRequest {
    pub target: TargetSpec,
    pub method: WipeMethod,
    /// Read back and check the final pass (default true).
    #[serde(default = "yes")]
    pub verify: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HashRequest {
    /// An image path or a disk (`disk4`).
    pub source: String,
    /// "sha256", "sha1", "md5" (default: all three).
    #[serde(default)]
    pub algorithms: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoctorAction {
    /// Report problems only.
    Check,
    /// Fix what can be fixed without moving data (GPT from its backup, CRCs, protective MBR…).
    Repair,
    ConvertToGpt,
    ConvertToMbr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoctorRequest {
    pub target: TargetSpec,
    pub action: DoctorAction,
}

/// Any request, tagged by `op`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Format(FormatRequest),
    Flash(FlashRequest),
    Wipe(WipeRequest),
    Hash(HashRequest),
    Doctor(DoctorRequest),
    /// Inspect a source image without writing anything (type, size, OS, recommended mode).
    Inspect { source: String },
}

impl Request {
    pub fn kind(&self) -> &'static str {
        match self {
            Request::Format(_) => "format",
            Request::Flash(_) => "flash",
            Request::Wipe(_) => "wipe",
            Request::Hash(_) => "hash",
            Request::Doctor(_) => "doctor",
            Request::Inspect { .. } => "inspect",
        }
    }
}

/// Run a request in the current thread (the job thread, or the CLI).
pub fn run(req: Request, ctx: &JobCtx) -> ForgeResult<Value> {
    match req {
        Request::Format(r) => super::format::run(&r, ctx),
        Request::Flash(r) => super::flash::run(&r, ctx),
        Request::Wipe(r) => super::wipe::run(&r, ctx),
        Request::Hash(r) => super::hash::run(&r, ctx),
        Request::Doctor(r) => super::doctor::run(&r, ctx),
        Request::Inspect { source } => super::image::inspect(&source, ctx),
    }
}

/// Start a request as a job; returns the job id.
pub fn start(req: Request) -> u64 {
    let kind = req.kind();
    super::job::spawn(kind, move |ctx| run(req, ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_as_the_app_sends_them() {
        let j = r#"{"op":"format","target":{"kind":"disk","bsd":"disk9"},"scheme":"gpt",
                    "partitions":[{"fs":"exfat","label":"DATI","cluster_size":131072}]}"#;
        let r: Request = serde_json::from_str(j).unwrap();
        let Request::Format(f) = &r else { panic!() };
        assert_eq!(f.partitions[0].fs, TargetFs::Exfat);
        assert_eq!(f.partitions[0].mkfs.cluster_size, Some(131072));
        assert!(f.clear_signatures);
        let j = r#"{"op":"flash","source":"/x.iso","target":{"kind":"file","path":"/tmp/o.img","size":null},
                    "windows":{"bypass_hardware_checks":true,"local_account":{"name":"mario"}}}"#;
        let Request::Flash(f) = serde_json::from_str::<Request>(j).unwrap() else { panic!() };
        assert_eq!(f.mode, WriteMode::Auto);
        assert!(f.verify && f.eject);
        assert!(f.windows.unwrap().bypass_hardware_checks);
    }
}
