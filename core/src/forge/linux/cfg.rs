//! Boot configuration rewriting for ISO mode (what Rufus does to isolinux / syslinux / GRUB /
//! systemd-boot files): volume-label references and persistence kernel parameters.

/// Whether `path` is a boot configuration file worth looking at.
pub fn is_config_path(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".cfg") || l.ends_with(".conf")
}

/// Lines whose label references are rewritten: kernel command lines (syslinux `append`,
/// GRUB `linux*`, systemd-boot `options`) and GRUB's `search` / `set` (root by label).
const LABEL_KEYWORDS: &[&str] = &["append", "linux", "linuxefi", "linux16", "kernel", "options", "search", "set"];

fn first_token(line: &str) -> String {
    line.split_whitespace().next().unwrap_or("").to_ascii_lowercase()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

fn boundary_before(c: Option<char>) -> bool {
    matches!(c, None | Some(' ' | '\t' | '=' | ':' | '\'' | '"'))
}
fn boundary_after(c: Option<char>) -> bool {
    matches!(c, None | Some(' ' | '\t' | '\'' | '"' | ',' | ';' | '\r' | '\n'))
}

/// Replace whole-word occurrences of `old` by `new` in `line`.
fn replace_word(line: &str, old: &str, new: &str) -> (String, usize) {
    if old.is_empty() {
        return (line.to_string(), 0);
    }
    let mut out = String::with_capacity(line.len());
    let mut n = 0usize;
    let mut i = 0usize;
    while let Some(p) = line[i..].find(old) {
        let at = i + p;
        let end = at + old.len();
        let before = line[..at].chars().next_back();
        let after = line[end..].chars().next();
        out.push_str(&line[i..at]);
        if boundary_before(before) && boundary_after(after) {
            out.push_str(new);
            n += 1;
        } else {
            out.push_str(old);
        }
        i = end;
    }
    out.push_str(&line[i..]);
    (out, n)
}

/// Rewrite references to the ISO's volume label `old` (also as GRUB / udev escape the spaces:
/// `\x20`, and octal `\040`) into `new`, on kernel command lines and GRUB search lines.
/// Returns the new text and the number of replacements.
pub fn replace_label(text: &str, old: &str, new: &str) -> (String, usize) {
    if old.is_empty() || old == new {
        return (text.to_string(), 0);
    }
    let mut variants = vec![(old.to_string(), new.to_string())];
    if old.contains(' ') || new.contains(' ') {
        variants.push((old.replace(' ', "\\x20"), new.replace(' ', "\\x20")));
        variants.push((old.replace(' ', "\\040"), new.replace(' ', "\\040")));
    }
    let mut out = String::with_capacity(text.len());
    let mut total = 0usize;
    for line in text.split_inclusive('\n') {
        if is_comment(line) || !LABEL_KEYWORDS.contains(&first_token(line).as_str()) {
            out.push_str(line);
            continue;
        }
        let mut l = line.to_string();
        for (o, n) in &variants {
            let (r, k) = replace_word(&l, o, n);
            l = r;
            total += k;
        }
        out.push_str(&l);
    }
    (out, total)
}

/// How a live system is told to use its persistence partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersistParams {
    /// Kernel parameter to add ("persistent" for casper, "persistence" for live-boot).
    pub param: &'static str,
    /// The boot parameter that identifies the live system's entries ("boot=casper").
    pub boot_token: &'static str,
    /// Directory of the live kernel ("/casper/"), for GRUB entries without `boot=`.
    pub kernel_dir: &'static str,
}

/// Add the persistence parameter to every live boot entry: syslinux `append`, GRUB `linux*`,
/// systemd-boot `options` lines that boot the live system (Rufus: after `boot=casper` /
/// `boot=live`, or after the kernel path for GRUB entries that have no `boot=`).
pub fn add_persistence(text: &str, p: &PersistParams) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + 64);
    let mut n = 0usize;
    for line in text.split_inclusive('\n') {
        if is_comment(line) {
            out.push_str(line);
            continue;
        }
        let tok = first_token(line);
        let words: Vec<&str> = line.split_whitespace().collect();
        let already = words.iter().any(|w| *w == p.param);
        let grub = matches!(tok.as_str(), "linux" | "linuxefi" | "linux16");
        let kernel_line = grub || matches!(tok.as_str(), "append" | "kernel" | "options");
        if !kernel_line || already {
            out.push_str(line);
            continue;
        }
        // Insert after `boot=…`, else (GRUB) after the kernel path when it is the live kernel.
        let insert_at = if let Some(pos) = find_word(line, p.boot_token) {
            Some(pos + p.boot_token.len())
        } else if grub && words.get(1).is_some_and(|k| k.contains(p.kernel_dir)) {
            words.get(1).and_then(|k| find_word(line, k)).map(|pos| pos + words[1].len())
        } else {
            None
        };
        match insert_at {
            Some(at) => {
                out.push_str(&line[..at]);
                out.push(' ');
                out.push_str(p.param);
                out.push_str(&line[at..]);
                n += 1;
            }
            None => out.push_str(line),
        }
    }
    (out, n)
}

/// Byte offset of `w` in `line` as a whitespace-separated word.
fn find_word(line: &str, w: &str) -> Option<usize> {
    let mut i = 0usize;
    while let Some(p) = line[i..].find(w) {
        let at = i + p;
        let before = line[..at].chars().next_back();
        let after = line[at + w.len()..].chars().next();
        if before.map_or(true, char::is_whitespace) && after.map_or(true, char::is_whitespace) {
            return Some(at);
        }
        i = at + w.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_references_are_rewritten_like_rufus() {
        let fedora = "menuentry 'Start Fedora-WS-Live-40-1-14' {\n\tlinux /images/pxeboot/vmlinuz root=live:CDLABEL=Fedora-WS-Live-40-1-14 rd.live.image quiet\n}\nsearch --no-floppy --set=root -l 'Fedora-WS-Live-40-1-14'\n# CDLABEL=Fedora-WS-Live-40-1-14 in a comment\n";
        let (out, n) = replace_label(fedora, "Fedora-WS-Live-40-1-14", "FEDORA-WS-L");
        assert_eq!(n, 2);
        assert!(out.contains("root=live:CDLABEL=FEDORA-WS-L rd.live"));
        assert!(out.contains("-l 'FEDORA-WS-L'"));
        assert!(out.contains("menuentry 'Start Fedora-WS-Live-40-1-14'"), "menu titles untouched");
        assert!(out.contains("# CDLABEL=Fedora-WS-Live-40-1-14"), "comments untouched");
        let arch = "LABEL arch\nAPPEND archisobasedir=arch archisolabel=ARCH_202411 cow_spacesize=1G\r\n";
        assert_eq!(replace_label(arch, "ARCH_202411", "ARCH_202411").1, 0);
        let (out, n) = replace_label(arch, "ARCH_202411", "ARCH_2024");
        assert_eq!(n, 1);
        assert!(out.ends_with("archisolabel=ARCH_2024 cow_spacesize=1G\r\n"), "{out:?}");
        let spaced = "linux /boot/vmlinuz root=live:LABEL=My\\x20Live\\x20Disc quiet\n";
        let (out, n) = replace_label(spaced, "My Live Disc", "MY LIVE");
        assert_eq!(n, 1);
        assert!(out.contains("LABEL=MY\\x20LIVE quiet"), "{out}");
        // Not a whole word: untouched.
        assert_eq!(replace_label("append x=ARCH_2024110\n", "ARCH_202411", "A").1, 0);
    }

    #[test]
    fn persistence_parameters_go_into_live_entries_only() {
        let casper = PersistParams { param: "persistent", boot_token: "boot=casper", kernel_dir: "/casper/" };
        let sys = "label live\n  kernel /casper/vmlinuz\n  append  file=/cdrom/preseed/ubuntu.seed boot=casper initrd=/casper/initrd quiet splash ---\nlabel memtest\n  kernel /install/mt86plus\n";
        let (out, n) = add_persistence(sys, &casper);
        assert_eq!(n, 1);
        assert!(out.contains("boot=casper persistent initrd=/casper/initrd"));
        assert_eq!(add_persistence(&out, &casper).1, 0, "idempotent");
        let grub = "menuentry \"Try Ubuntu\" {\n\tlinux\t/casper/vmlinuz  --- quiet splash\n\tinitrd\t/casper/initrd\n}\nmenuentry x {\n\tlinux /boot/memtest\n}\n";
        let (out, n) = add_persistence(grub, &casper);
        assert_eq!(n, 1);
        assert!(out.contains("linux\t/casper/vmlinuz persistent  --- quiet"), "{out}");
        let live = PersistParams { param: "persistence", boot_token: "boot=live", kernel_dir: "/live/" };
        let deb = "\tlinux\t/live/vmlinuz-6.12 boot=live components quiet splash findiso=${iso_path}\n# linux /live/vmlinuz boot=live custom options\n\tappend boot=live components\n";
        let (out, n) = add_persistence(deb, &live);
        assert_eq!(n, 2);
        assert!(out.contains("boot=live persistence components quiet"));
        assert!(out.contains("# linux /live/vmlinuz boot=live custom options"));
        let sdboot = "title Arch\noptions archisobasedir=arch boot=live\n";
        assert!(add_persistence(sdboot, &live).0.contains("boot=live persistence\n"));
    }
}
