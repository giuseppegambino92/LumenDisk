//! `autounattend.xml` for Windows Setup, generated the way Rufus 4.15 does it
//! (`src/wue.c`, `CreateUnattendXml`), from the user's [`WindowsOptions`]:
//!
//! - `bypass_hardware_checks` (Windows 11): `windowsPE` pass, `Microsoft-Windows-Setup`
//!   `RunSynchronous` commands that set `HKLM\SYSTEM\Setup\LabConfig` `BypassTPMCheck`,
//!   `BypassSecureBootCheck`, `BypassRAMCheck`, `BypassCPUCheck`, `BypassStorageCheck` = 1
//!   before Setup checks the hardware. Like Rufus, the pass also carries `UserData` with
//!   `AcceptEula` and an empty `ProductKey` ("WinPE will complain if we don't provide a
//!   product key").
//! - `local_account`: `specialize` pass `BypassNRO` (Windows 11: no network / Microsoft
//!   account required), and `oobeSystem` `Microsoft-Windows-Shell-Setup` `UserAccounts` /
//!   `LocalAccounts` with the password encoded as the unattend schema wants it (base64 of the
//!   UTF-16LE string `password + "Password"`, `PlainText` false), `HideOnlineAccountScreens`,
//!   `HideLocalAccountScreen`, `HideEULAPage`. With an empty password, Rufus's first-logon
//!   commands make Windows ask for a new password and keep it from expiring.
//! - `skip_privacy_questions`: `OOBE/ProtectYourPC` = 3 (no optional data collection) and
//!   `HideEULAPage`; on Windows 10 1809+ also the `DisablePrivacyExperience` policy, which skips
//!   the privacy settings pages.
//! - `locale` / `time_zone`: `Microsoft-Windows-International-Core` (`InputLocale`,
//!   `SystemLocale`, `UILanguage`, `UserLocale`) and `Shell-Setup/TimeZone` in `oobeSystem`.
//! - `custom_answer_file`: written verbatim once it is checked to be well-formed XML.
//!
//! Schema reference: Microsoft, "Unattended Windows Setup Reference"
//! (learn.microsoft.com/windows-hardware/customize/desktop/unattend/): components
//! `microsoft-windows-setup-runsynchronous`, `microsoft-windows-deployment-runsynchronous`,
//! `microsoft-windows-shell-setup-useraccounts-localaccounts-localaccount-password` ("the
//! password … appended with the string Password, then Base64 encoded" for `PlainText` false),
//! `microsoft-windows-shell-setup-oobe-*`, `microsoft-windows-international-core-*`.
//!
//! Components carry `processorArchitecture` = the media's architecture (`x86`, `amd64`,
//! `arm64`); when it is unknown, each component is emitted once per architecture, as Windows
//! Setup ignores the ones that don't match. Settings a Windows version doesn't know are left
//! out (and reported): the LabConfig bypass and `BypassNRO` exist for Windows 11 only,
//! `HideOnlineAccountScreens` / `HideLocalAccountScreen` from Windows 8, the
//! `DisablePrivacyExperience` policy from Windows 10 1809.

use super::super::plan::WindowsOptions;
use super::super::{ForgeError, ForgeResult};
use crate::i18n::t;

/// Processor architecture of the media.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WinArch {
    X86,
    X64,
    Arm64,
}

impl WinArch {
    pub const ALL: [WinArch; 3] = [WinArch::X86, WinArch::X64, WinArch::Arm64];

    /// `processorArchitecture` value.
    pub fn unattend_name(self) -> &'static str {
        match self {
            WinArch::X86 => "x86",
            WinArch::X64 => "amd64",
            WinArch::Arm64 => "arm64",
        }
    }

    /// What the UI shows.
    pub fn label(self) -> &'static str {
        match self {
            WinArch::X86 => "x86",
            WinArch::X64 => "x64",
            WinArch::Arm64 => "arm64",
        }
    }

    /// The WIM XML `<ARCH>` value (`PROCESSOR_ARCHITECTURE_*`).
    pub fn from_wim(n: u32) -> Option<WinArch> {
        match n {
            0 => Some(WinArch::X86),
            9 => Some(WinArch::X64),
            12 => Some(WinArch::Arm64),
            _ => None,
        }
    }
}

/// What the answer file needs to know about the media.
#[derive(Clone, Debug, Default)]
pub struct Media {
    pub arch: Option<WinArch>,
    /// Build number (22000+ = Windows 11); None = unknown (everything is emitted).
    pub build: Option<u32>,
    pub server: bool,
    /// Languages of the install image (`<LANGUAGES>`), default first.
    pub languages: Vec<String>,
}

impl Media {
    fn windows11(&self) -> bool {
        !self.server && self.build.is_none_or(|b| b >= 22000)
    }
    fn windows8_or_later(&self) -> bool {
        self.build.is_none_or(|b| b >= 9200)
    }
    fn privacy_policy(&self) -> bool {
        self.build.is_none_or(|b| b >= 17763)
    }
}

/// A generated (or user-supplied) answer file.
#[derive(Clone, Debug)]
pub struct Answer {
    pub xml: String,
    pub custom: bool,
    /// Options left out or adjusted, for the job log.
    pub notes: Vec<String>,
}

/// Whether any option asks for an answer file.
pub fn wanted(o: &WindowsOptions) -> bool {
    o.custom_answer_file.is_some() || o.bypass_hardware_checks || o.local_account.is_some() || o.skip_privacy_questions || o.locale.is_some() || o.time_zone.is_some()
}

/// The `LabConfig` values Setup checks (Windows 11).
pub const LABCONFIG_BYPASSES: [&str; 5] = ["BypassTPMCheck", "BypassSecureBootCheck", "BypassRAMCheck", "BypassCPUCheck", "BypassStorageCheck"];

/// Names Windows won't accept for a new local account (Rufus `wue.c`
/// `unallowed_account_names`, from Microsoft's list of localized administrator names).
const RESERVED_NAMES: [&str; 15] = [
    "Administrator",
    "Järjestelmänvalvoja",
    "Administrateur",
    "Rendszergazda",
    "Administrador",
    "Администратор",
    "Administratör",
    "Guest",
    "DefaultAccount",
    "WDAGUtilityAccount",
    "HelpAssistant",
    "KRBTGT",
    "Local",
    "NONE",
    "SYSTEM",
];
/// Rufus `USERNAME_INVALID_CHARS`.
const INVALID_NAME_CHARS: &str = "/\\[]:;|=.,+*?<>%@&\"";

/// The unattend encoding of a `LocalAccount` password: base64(UTF-16LE(password + "Password")).
pub fn encode_password(password: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = format!("{password}Password").encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// XML-escape text and attribute values.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

fn invalid(it: &str, en: &str) -> ForgeError {
    ForgeError::Invalid(t(it, en))
}

/// The account name as Windows will take it (Rufus: trimmed, invalid characters → `_`), or why
/// it can't be used.
pub fn account_name(name: &str, notes: &mut Vec<String>) -> ForgeResult<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(invalid("il nome dell'account locale è vuoto", "the local account name is empty"));
    }
    if RESERVED_NAMES.iter().any(|r| r.to_lowercase() == trimmed.to_lowercase()) {
        return Err(invalid(&format!("«{trimmed}» non può essere usato come nome dell'account locale"), &format!("'{trimmed}' can't be used as the local account name")));
    }
    let clean: String = trimmed.chars().map(|c| if INVALID_NAME_CHARS.contains(c) || c.is_control() { '_' } else { c }).collect();
    if clean.chars().count() > 20 {
        return Err(invalid("il nome dell'account locale supera i 20 caratteri", "the local account name is longer than 20 characters"));
    }
    if clean != trimmed {
        notes.push(format!("local account name sanitized to '{clean}'"));
    }
    Ok(clean)
}

/// A language tag like `it-IT`, `en-US`, `zh-Hant-TW`, `sr-Latn-RS`.
fn valid_locale(s: &str) -> bool {
    let mut parts = s.split('-');
    let first = parts.next().unwrap_or("");
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && s.len() <= 32
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Windows time zone id for `tz`: Windows ids pass through, IANA names are mapped (CLDR
/// `windowsZones.xml`, the zones people pick most).
pub fn windows_time_zone(tz: &str) -> ForgeResult<String> {
    let tz = tz.trim();
    if let Some((_, w)) = IANA_TO_WINDOWS.iter().find(|(i, _)| i.eq_ignore_ascii_case(tz)) {
        return Ok(w.to_string());
    }
    let windows_id = !tz.is_empty() && tz.len() <= 64 && !tz.contains('/') && tz.chars().all(|c| c.is_ascii_alphanumeric() || " .()+-_".contains(c));
    if windows_id {
        return Ok(tz.to_string());
    }
    Err(invalid(&format!("fuso orario sconosciuto: «{tz}»"), &format!("unknown time zone: '{tz}'")))
}

const IANA_TO_WINDOWS: &[(&str, &str)] = &[
    ("UTC", "UTC"),
    ("Etc/UTC", "UTC"),
    ("Etc/GMT", "UTC"),
    ("Europe/Rome", "W. Europe Standard Time"),
    ("Europe/Vatican", "W. Europe Standard Time"),
    ("Europe/San_Marino", "W. Europe Standard Time"),
    ("Europe/Malta", "W. Europe Standard Time"),
    ("Europe/Berlin", "W. Europe Standard Time"),
    ("Europe/Amsterdam", "W. Europe Standard Time"),
    ("Europe/Vienna", "W. Europe Standard Time"),
    ("Europe/Zurich", "W. Europe Standard Time"),
    ("Europe/Stockholm", "W. Europe Standard Time"),
    ("Europe/Oslo", "W. Europe Standard Time"),
    ("Europe/Luxembourg", "W. Europe Standard Time"),
    ("Europe/Monaco", "W. Europe Standard Time"),
    ("Europe/Andorra", "W. Europe Standard Time"),
    ("Europe/Gibraltar", "W. Europe Standard Time"),
    ("Europe/Paris", "Romance Standard Time"),
    ("Europe/Brussels", "Romance Standard Time"),
    ("Europe/Madrid", "Romance Standard Time"),
    ("Europe/Copenhagen", "Romance Standard Time"),
    ("Europe/Prague", "Central Europe Standard Time"),
    ("Europe/Budapest", "Central Europe Standard Time"),
    ("Europe/Bratislava", "Central Europe Standard Time"),
    ("Europe/Ljubljana", "Central Europe Standard Time"),
    ("Europe/Belgrade", "Central Europe Standard Time"),
    ("Europe/Tirane", "Central Europe Standard Time"),
    ("Europe/Podgorica", "Central Europe Standard Time"),
    ("Europe/Warsaw", "Central European Standard Time"),
    ("Europe/Zagreb", "Central European Standard Time"),
    ("Europe/Sarajevo", "Central European Standard Time"),
    ("Europe/Skopje", "Central European Standard Time"),
    ("Europe/London", "GMT Standard Time"),
    ("Europe/Dublin", "GMT Standard Time"),
    ("Europe/Lisbon", "GMT Standard Time"),
    ("Atlantic/Reykjavik", "Greenwich Standard Time"),
    ("Europe/Athens", "GTB Standard Time"),
    ("Europe/Bucharest", "GTB Standard Time"),
    ("Europe/Helsinki", "FLE Standard Time"),
    ("Europe/Kiev", "FLE Standard Time"),
    ("Europe/Kyiv", "FLE Standard Time"),
    ("Europe/Riga", "FLE Standard Time"),
    ("Europe/Sofia", "FLE Standard Time"),
    ("Europe/Tallinn", "FLE Standard Time"),
    ("Europe/Vilnius", "FLE Standard Time"),
    ("Europe/Chisinau", "E. Europe Standard Time"),
    ("Europe/Istanbul", "Turkey Standard Time"),
    ("Europe/Moscow", "Russian Standard Time"),
    ("Europe/Minsk", "Belarus Standard Time"),
    ("Africa/Cairo", "Egypt Standard Time"),
    ("Africa/Johannesburg", "South Africa Standard Time"),
    ("Africa/Lagos", "W. Central Africa Standard Time"),
    ("Africa/Nairobi", "E. Africa Standard Time"),
    ("Africa/Casablanca", "Morocco Standard Time"),
    ("Asia/Jerusalem", "Israel Standard Time"),
    ("Asia/Dubai", "Arabian Standard Time"),
    ("Asia/Riyadh", "Arab Standard Time"),
    ("Asia/Tehran", "Iran Standard Time"),
    ("Asia/Karachi", "Pakistan Standard Time"),
    ("Asia/Kolkata", "India Standard Time"),
    ("Asia/Calcutta", "India Standard Time"),
    ("Asia/Kathmandu", "Nepal Standard Time"),
    ("Asia/Dhaka", "Bangladesh Standard Time"),
    ("Asia/Bangkok", "SE Asia Standard Time"),
    ("Asia/Jakarta", "SE Asia Standard Time"),
    ("Asia/Ho_Chi_Minh", "SE Asia Standard Time"),
    ("Asia/Shanghai", "China Standard Time"),
    ("Asia/Hong_Kong", "China Standard Time"),
    ("Asia/Singapore", "Singapore Standard Time"),
    ("Asia/Kuala_Lumpur", "Singapore Standard Time"),
    ("Asia/Manila", "Singapore Standard Time"),
    ("Asia/Taipei", "Taipei Standard Time"),
    ("Asia/Tokyo", "Tokyo Standard Time"),
    ("Asia/Seoul", "Korea Standard Time"),
    ("Australia/Perth", "W. Australia Standard Time"),
    ("Australia/Adelaide", "Cen. Australia Standard Time"),
    ("Australia/Darwin", "AUS Central Standard Time"),
    ("Australia/Brisbane", "E. Australia Standard Time"),
    ("Australia/Sydney", "AUS Eastern Standard Time"),
    ("Australia/Melbourne", "AUS Eastern Standard Time"),
    ("Australia/Hobart", "Tasmania Standard Time"),
    ("Pacific/Auckland", "New Zealand Standard Time"),
    ("Pacific/Honolulu", "Hawaiian Standard Time"),
    ("America/Anchorage", "Alaskan Standard Time"),
    ("America/Los_Angeles", "Pacific Standard Time"),
    ("America/Vancouver", "Pacific Standard Time"),
    ("America/Phoenix", "US Mountain Standard Time"),
    ("America/Denver", "Mountain Standard Time"),
    ("America/Edmonton", "Mountain Standard Time"),
    ("America/Chicago", "Central Standard Time"),
    ("America/Winnipeg", "Central Standard Time"),
    ("America/Mexico_City", "Central Standard Time (Mexico)"),
    ("America/New_York", "Eastern Standard Time"),
    ("America/Toronto", "Eastern Standard Time"),
    ("America/Detroit", "Eastern Standard Time"),
    ("America/Halifax", "Atlantic Standard Time"),
    ("America/St_Johns", "Newfoundland Standard Time"),
    ("America/Sao_Paulo", "E. South America Standard Time"),
    ("America/Argentina/Buenos_Aires", "Argentina Standard Time"),
    ("America/Buenos_Aires", "Argentina Standard Time"),
    ("America/Bogota", "SA Pacific Standard Time"),
    ("America/Lima", "SA Pacific Standard Time"),
    ("America/Caracas", "Venezuela Standard Time"),
    ("America/Santiago", "Pacific SA Standard Time"),
];

/// Check that a user-supplied answer file is well-formed XML with one root element.
pub fn check_well_formed(xml: &str) -> ForgeResult<()> {
    use quick_xml::events::Event;
    let bad = |why: String| invalid(&format!("il file di risposte non è XML valido: {why}"), &format!("the answer file is not well-formed XML: {why}"));
    if xml.len() > 4 << 20 {
        return Err(bad("larger than 4 MiB".into()));
    }
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().check_end_names = true;
    reader.config_mut().check_comments = true;
    let mut depth = 0usize;
    let mut roots = 0usize;
    let check_attrs = |e: &quick_xml::events::BytesStart| -> Result<(), String> {
        for a in e.attributes().with_checks(true) {
            let a = a.map_err(|e| e.to_string())?;
            a.unescape_value().map_err(|e| e.to_string())?;
        }
        Ok(())
    };
    loop {
        let pos = reader.buffer_position();
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                check_attrs(&e).map_err(bad)?;
                if depth == 0 {
                    roots += 1;
                }
                depth += 1;
                if depth > 256 {
                    return Err(bad("nested too deeply".into()));
                }
            }
            Ok(Event::Empty(e)) => {
                check_attrs(&e).map_err(bad)?;
                if depth == 0 {
                    roots += 1;
                }
            }
            Ok(Event::End(_)) => depth -= 1,
            Ok(Event::Text(txt)) => {
                txt.unescape().map_err(|e| bad(e.to_string()))?;
                if depth == 0 && !txt.iter().all(u8::is_ascii_whitespace) && !(pos == 0 && txt.starts_with("\u{feff}".as_bytes())) {
                    return Err(bad("text outside the root element".into()));
                }
            }
            Ok(Event::CData(_)) if depth == 0 => return Err(bad("CDATA outside the root element".into())),
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(bad(format!("{e} (byte {pos})"))),
        }
    }
    if depth != 0 {
        return Err(bad("unclosed elements at the end".into()));
    }
    if roots != 1 {
        return Err(bad(format!("{roots} root elements")));
    }
    Ok(())
}

/// Builds the XML with two-space indentation.
struct Doc {
    s: String,
    arches: Vec<WinArch>,
}

impl Doc {
    fn line(&mut self, indent: usize, text: &str) {
        for _ in 0..indent {
            self.s.push_str("  ");
        }
        self.s.push_str(text);
        self.s.push('\n');
    }

    fn elem(&mut self, indent: usize, tag: &str, value: &str) {
        self.line(indent, &format!("<{tag}>{}</{tag}>", esc(value)));
    }

    /// One `<component>` per architecture, each with the same body.
    fn component(&mut self, name: &str, body: &dyn Fn(&mut Doc)) {
        for arch in self.arches.clone() {
            self.line(
                2,
                &format!(
                    "<component name=\"{name}\" processorArchitecture=\"{}\" publicKeyToken=\"31bf3856ad364e35\" language=\"neutral\" versionScope=\"nonSxS\" xmlns:wcm=\"http://schemas.microsoft.com/WMIConfig/2002/State\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">",
                    arch.unattend_name()
                ),
            );
            body(self);
            self.line(2, "</component>");
        }
    }
}

/// Generate the answer file for `o` (None: no option needs one, or none applies to this
/// Windows version).
pub fn build(o: &WindowsOptions, m: &Media) -> ForgeResult<Option<Answer>> {
    if let Some(custom) = &o.custom_answer_file {
        check_well_formed(custom)?;
        let mut notes = vec![];
        if o.bypass_hardware_checks || o.local_account.is_some() || o.skip_privacy_questions || o.locale.is_some() || o.time_zone.is_some() {
            notes.push("custom answer file supplied: the other unattended options are not applied".into());
        }
        return Ok(Some(Answer { xml: custom.clone(), custom: true, notes }));
    }
    let mut notes = vec![];

    // Validate everything first.
    let account = match &o.local_account {
        Some(a) => {
            let name = account_name(&a.name, &mut notes)?;
            if a.password.chars().count() > 127 {
                return Err(invalid("la password supera i 127 caratteri", "the password is longer than 127 characters"));
            }
            Some((name, a.password.clone()))
        }
        None => None,
    };
    let locale = match &o.locale {
        Some(l) if !l.trim().is_empty() => {
            let l = l.trim();
            if !valid_locale(l) {
                return Err(invalid(&format!("lingua non valida: «{l}»"), &format!("invalid locale: '{l}'")));
            }
            Some(l.to_string())
        }
        _ => None,
    };
    let tz = match &o.time_zone {
        Some(z) if !z.trim().is_empty() => Some(windows_time_zone(z)?),
        _ => None,
    };

    // What applies to this Windows.
    let bypass = o.bypass_hardware_checks && m.windows11();
    if o.bypass_hardware_checks && !bypass {
        notes.push("hardware-check bypass skipped: only Windows 11 checks TPM / Secure Boot / CPU".into());
    }
    let mut specialize: Vec<String> = vec![];
    if account.is_some() && m.windows11() {
        specialize.push(r#"reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\OOBE" /v BypassNRO /t REG_DWORD /d 1 /f"#.into());
    }
    if o.skip_privacy_questions && m.privacy_policy() {
        specialize.push(r#"reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows\OOBE" /v DisablePrivacyExperience /t REG_DWORD /d 1 /f"#.into());
    }
    let hide_eula = o.skip_privacy_questions || account.is_some();
    let hide_accounts = account.is_some() && m.windows8_or_later();
    let shell_setup = hide_eula || tz.is_some() || account.is_some();
    if !bypass && specialize.is_empty() && !shell_setup && locale.is_none() {
        return Ok(None);
    }

    let mut d = Doc { s: String::new(), arches: m.arch.map(|a| vec![a]).unwrap_or_else(|| WinArch::ALL.to_vec()) };
    d.line(0, r#"<?xml version="1.0" encoding="utf-8"?>"#);
    d.line(0, r#"<unattend xmlns="urn:schemas-microsoft-com:unattend">"#);

    if bypass {
        d.line(1, r#"<settings pass="windowsPE">"#);
        d.component("Microsoft-Windows-Setup", &|d| {
            d.line(3, "<UserData>");
            d.elem(4, "AcceptEula", "true");
            d.line(4, "<ProductKey>");
            d.line(5, "<Key />");
            d.line(4, "</ProductKey>");
            d.line(3, "</UserData>");
            d.line(3, "<RunSynchronous>");
            for (i, name) in LABCONFIG_BYPASSES.iter().enumerate() {
                d.line(4, r#"<RunSynchronousCommand wcm:action="add">"#);
                d.elem(5, "Order", &(i + 1).to_string());
                d.elem(5, "Path", &format!(r"reg add HKLM\SYSTEM\Setup\LabConfig /v {name} /t REG_DWORD /d 1 /f"));
                d.line(4, "</RunSynchronousCommand>");
            }
            d.line(3, "</RunSynchronous>");
        });
        d.line(1, "</settings>");
    }

    if !specialize.is_empty() {
        d.line(1, r#"<settings pass="specialize">"#);
        d.component("Microsoft-Windows-Deployment", &|d| {
            d.line(3, "<RunSynchronous>");
            for (i, cmd) in specialize.iter().enumerate() {
                d.line(4, r#"<RunSynchronousCommand wcm:action="add">"#);
                d.elem(5, "Order", &(i + 1).to_string());
                d.elem(5, "Path", cmd);
                d.line(4, "</RunSynchronousCommand>");
            }
            d.line(3, "</RunSynchronous>");
        });
        d.line(1, "</settings>");
    }

    if shell_setup || locale.is_some() {
        d.line(1, r#"<settings pass="oobeSystem">"#);
        if let Some(l) = &locale {
            // Setup can only show a language the image has: otherwise keep its default.
            let ui = if m.languages.is_empty() || m.languages.iter().any(|x| x.eq_ignore_ascii_case(l)) {
                l.clone()
            } else {
                notes.push(format!("display language {l} is not in the image; using {}", m.languages[0]));
                m.languages[0].clone()
            };
            d.component("Microsoft-Windows-International-Core", &|d| {
                d.elem(3, "InputLocale", l);
                d.elem(3, "SystemLocale", l);
                d.elem(3, "UILanguage", &ui);
                d.elem(3, "UserLocale", l);
            });
        }
        if shell_setup {
            let first_logon: Vec<String> = match &account {
                Some((name, pw)) if pw.is_empty() => vec![format!("net user \"{name}\" /logonpasswordchg:yes"), "net accounts /maxpwage:unlimited".into()],
                _ => vec![],
            };
            d.component("Microsoft-Windows-Shell-Setup", &|d| {
                if hide_eula || hide_accounts || o.skip_privacy_questions {
                    d.line(3, "<OOBE>");
                    if hide_eula {
                        d.elem(4, "HideEULAPage", "true");
                    }
                    if hide_accounts {
                        d.elem(4, "HideLocalAccountScreen", "true");
                        d.elem(4, "HideOnlineAccountScreens", "true");
                    }
                    if o.skip_privacy_questions {
                        d.elem(4, "ProtectYourPC", "3");
                    }
                    d.line(3, "</OOBE>");
                }
                if let Some(z) = &tz {
                    d.elem(3, "TimeZone", z);
                }
                if let Some((name, pw)) = &account {
                    d.line(3, "<UserAccounts>");
                    d.line(4, "<LocalAccounts>");
                    d.line(5, r#"<LocalAccount wcm:action="add">"#);
                    d.elem(6, "Name", name);
                    d.elem(6, "DisplayName", name);
                    d.elem(6, "Group", "Administrators;Power Users");
                    d.line(6, "<Password>");
                    d.elem(7, "Value", &encode_password(pw));
                    d.elem(7, "PlainText", "false");
                    d.line(6, "</Password>");
                    d.line(5, "</LocalAccount>");
                    d.line(4, "</LocalAccounts>");
                    d.line(3, "</UserAccounts>");
                }
                if !first_logon.is_empty() {
                    d.line(3, "<FirstLogonCommands>");
                    for (i, cmd) in first_logon.iter().enumerate() {
                        d.line(4, r#"<SynchronousCommand wcm:action="add">"#);
                        d.elem(5, "Order", &(i + 1).to_string());
                        d.elem(5, "CommandLine", cmd);
                        d.line(4, "</SynchronousCommand>");
                    }
                    d.line(3, "</FirstLogonCommands>");
                }
            });
        }
        d.line(1, "</settings>");
    }
    d.line(0, "</unattend>");
    check_well_formed(&d.s)?;
    Ok(Some(Answer { xml: d.s, custom: false, notes }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::plan::LocalAccount;

    fn opts() -> WindowsOptions {
        WindowsOptions::default()
    }

    fn win11() -> Media {
        Media { arch: Some(WinArch::X64), build: Some(26100), server: false, languages: vec!["en-US".into()] }
    }

    /// Compare with `testdata/<name>.xml`; `LUMENDISK_UPDATE_GOLDEN=1` rewrites it.
    fn golden(name: &str, xml: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/forge/windows/testdata").join(format!("{name}.xml"));
        if std::env::var("LUMENDISK_UPDATE_GOLDEN").as_deref() == Ok("1") {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, xml).unwrap();
        }
        let want = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing golden file {}", path.display()));
        assert_eq!(xml, want, "{name}: output differs from the golden file");
    }

    #[test]
    fn password_encoding_follows_the_unattend_spec() {
        // Rufus's constant for an empty password.
        assert_eq!(encode_password(""), "UABhAHMAcwB3AG8AcgBkAA==");
        assert_eq!(encode_password("Ciao!"), "QwBpAGEAbwAhAFAAYQBzAHMAdwBvAHIAZAA=");
    }

    #[test]
    fn nothing_requested_means_no_file() {
        assert!(!wanted(&opts()));
        assert!(build(&opts(), &win11()).unwrap().is_none());
        // Only a Windows 11 option on Windows 10: nothing left to write.
        let o = WindowsOptions { bypass_hardware_checks: true, ..opts() };
        let a = build(&o, &Media { build: Some(19045), ..win11() }).unwrap();
        assert!(a.is_none());
    }

    #[test]
    fn golden_windows11_everything() {
        let o = WindowsOptions {
            bypass_hardware_checks: true,
            local_account: Some(LocalAccount { name: "mario".into(), password: "s3gr&to<".into() }),
            skip_privacy_questions: true,
            locale: Some("it-IT".into()),
            time_zone: Some("Europe/Rome".into()),
            ..opts()
        };
        let m = Media { languages: vec!["it-IT".into()], ..win11() };
        let a = build(&o, &m).unwrap().unwrap();
        assert!(a.notes.is_empty(), "{:?}", a.notes);
        golden("win11_all", &a.xml);
    }

    #[test]
    fn golden_windows11_bypass_only_unknown_arch() {
        let o = WindowsOptions { bypass_hardware_checks: true, ..opts() };
        let a = build(&o, &Media { arch: None, ..win11() }).unwrap().unwrap();
        assert_eq!(a.xml.matches("<component ").count(), 3, "one per architecture");
        golden("win11_bypass_any_arch", &a.xml);
    }

    #[test]
    fn golden_windows10_local_account_empty_password() {
        let o = WindowsOptions {
            bypass_hardware_checks: true,
            local_account: Some(LocalAccount { name: " Luigi ".into(), password: String::new() }),
            locale: Some("fr-FR".into()),
            ..opts()
        };
        let m = Media { arch: Some(WinArch::X86), build: Some(19045), server: false, languages: vec!["en-US".into()] };
        let a = build(&o, &m).unwrap().unwrap();
        assert!(!a.xml.contains("LabConfig") && !a.xml.contains("BypassNRO"), "Windows 11 only");
        assert!(a.notes.iter().any(|n| n.contains("fr-FR")), "{:?}", a.notes);
        assert!(a.xml.contains("<UILanguage>en-US</UILanguage>") && a.xml.contains("<UserLocale>fr-FR</UserLocale>"));
        golden("win10_local_account", &a.xml);
    }

    #[test]
    fn golden_arm64_privacy_and_time_zone() {
        let o = WindowsOptions { skip_privacy_questions: true, time_zone: Some("Pacific Standard Time".into()), ..opts() };
        let m = Media { arch: Some(WinArch::Arm64), ..win11() };
        golden("win11_arm64_privacy", &build(&o, &m).unwrap().unwrap().xml);
    }

    #[test]
    fn user_input_is_escaped_or_refused() {
        let o = WindowsOptions { local_account: Some(LocalAccount { name: "a<b>&\"c'".into(), password: "x".into() }), ..opts() };
        let a = build(&o, &win11()).unwrap().unwrap();
        check_well_formed(&a.xml).unwrap();
        assert!(a.xml.contains("<Name>a_b___c&apos;</Name>"), "{}", a.xml);
        assert!(a.notes.iter().any(|n| n.contains("sanitized")));
        for bad in ["Administrator", "guest", "", "   ", "abcdefghijklmnopqrstu"] {
            let o = WindowsOptions { local_account: Some(LocalAccount { name: bad.into(), password: String::new() }), ..opts() };
            assert!(build(&o, &win11()).is_err(), "{bad:?}");
        }
        for bad in ["it_IT", "<x>", "toolonglanguagecode-xx", ""] {
            let o = WindowsOptions { locale: Some(bad.into()), ..opts() };
            let r = build(&o, &win11());
            if bad.is_empty() {
                assert!(r.unwrap().is_none());
            } else {
                assert!(r.is_err(), "{bad:?}");
            }
        }
        assert!(windows_time_zone("Mars/Olympus").is_err());
        assert!(windows_time_zone("W. Europe <Standard>").is_err());
        assert_eq!(windows_time_zone("europe/rome").unwrap(), "W. Europe Standard Time");
        assert_eq!(windows_time_zone("UTC").unwrap(), "UTC");
    }

    #[test]
    fn custom_files_are_checked_and_kept_verbatim() {
        let good = "\u{feff}<?xml version=\"1.0\"?>\n<!-- mine -->\n<unattend xmlns=\"urn:schemas-microsoft-com:unattend\"><settings pass=\"oobeSystem\"/></unattend>\n";
        let o = WindowsOptions { custom_answer_file: Some(good.into()), bypass_hardware_checks: true, ..opts() };
        let a = build(&o, &win11()).unwrap().unwrap();
        assert!(a.custom);
        assert_eq!(a.xml, good);
        assert_eq!(a.notes.len(), 1);
        for bad in ["<unattend>", "<a></b>", "<a/><b/>", "text", "<a x=\"1\" x=\"2\"/>", "<a>&nope;</a>", "<a></a>trailing", "<a x=1/>"] {
            let o = WindowsOptions { custom_answer_file: Some(bad.into()), ..opts() };
            assert!(build(&o, &win11()).is_err(), "{bad:?} accepted");
        }
    }
}
