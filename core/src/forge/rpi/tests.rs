use super::*;
use crate::block::MemDev;
use crate::forge::plan::Wifi;
use crate::vfs::FsType;
use sha2::{Digest, Sha256};

/// A fixed hash (not a real one) so generated files are reproducible.
const HASH: &str = "$6$LumenDiskTest$golden";

fn full() -> RpiOptions {
    RpiOptions {
        hostname: Some("pi-lab".into()),
        user: Some("mario".into()),
        password: Some("Tr0ub4dor&3\n".into()),
        ssh: true,
        ssh_authorized_keys: Some("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGx1bWVuZGlzay10ZXN0LWtleS0wMDAwMDAw mario@mac\n\n# comment\nssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQC7 it's-me\n".into()),
        wifi: Some(Wifi { ssid: "Casa \"Rossi\" ✓".into(), password: "p@ss w'rd\\$(x)".into(), country: "it".into(), hidden: true }),
        locale: Some("it_IT.UTF-8".into()),
        time_zone: Some("Europe/Rome".into()),
        keyboard_layout: Some("it".into()),
    }
}

/// Compare with a golden file in `rpi/golden/`; `LUMENDISK_BLESS=1` rewrites it.
fn golden(name: &str, got: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/forge/rpi/golden").join(name);
    if std::env::var("LUMENDISK_BLESS").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, got).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing golden file {name} (run with LUMENDISK_BLESS=1)"));
    assert_eq!(got, want, "{name} differs from its golden file");
}

#[test]
fn validation_accepts_good_input_and_names_the_bad_field() {
    let s = validate(&full()).unwrap();
    assert_eq!(s.password.as_deref(), Some("Tr0ub4dor&3"), "CR/LF stripped");
    assert_eq!(s.keys.len(), 2, "blank and comment lines skipped");
    assert_eq!(s.wifi.as_ref().unwrap().country, "IT", "upper-cased");
    assert!(validate(&RpiOptions::default()).unwrap().is_empty());

    for h in ["a", "pi-lab", "x1", &"a".repeat(63), "123"] {
        assert!(valid_hostname(h), "{h}");
    }
    for h in ["", "-a", "a-", "a_b", "a.b", "à", &"a".repeat(64), "a b", "a;reboot"] {
        assert!(!valid_hostname(h), "{h}");
    }
    for u in ["mario", "pi", "a1-b", &"a".repeat(32)] {
        assert!(valid_username(u), "{u}");
    }
    for u in ["", "1abc", "Mario", "ma_rio", "root", "daemon", "systemd-x", &"a".repeat(33), "a b", "a:b", "a$"] {
        assert!(!valid_username(u), "{u}");
    }
    assert!(valid_wpa_key("12345678") && valid_wpa_key(&"x".repeat(63)) && valid_wpa_key(&"aB".repeat(32)));
    for p in ["1234567", &"x".repeat(64), "passwörd1", "tab\there1"] {
        assert!(!valid_wpa_key(p), "{p}");
    }
    for l in ["it_IT.UTF-8", "en_GB.UTF-8", "C.UTF-8", "C", "sr_RS@latin", "de_DE"] {
        assert!(valid_locale(l), "{l}");
    }
    for l in ["", "it_IT.UTF-8; rm", "IT_it", "x", "it_IT.UTF-8\n"] {
        assert!(!valid_locale(l), "{l}");
    }
    for z in ["Europe/Rome", "UTC", "America/Argentina/Buenos_Aires", "Etc/GMT+1"] {
        assert!(valid_time_zone(z), "{z}");
    }
    for z in ["", "../etc/passwd", "Europe//Rome", "Europe/Rome'", "-x"] {
        assert!(!valid_time_zone(z), "{z}");
    }
    assert!(valid_keyboard("it") && valid_keyboard("latam") && !valid_keyboard("it'") && !valid_keyboard("IT"));
    assert!(valid_ssh_key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGx1 me"));
    assert!(valid_ssh_key("from=\"10.0.0.1\" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGx1"));
    assert!(!valid_ssh_key("ssh-ed25519"), "no key");
    assert!(!valid_ssh_key("hello world this is not a key"));

    let bad = |f: &dyn Fn(&mut RpiOptions), what: &str| {
        let mut o = full();
        f(&mut o);
        let e = validate(&o).unwrap_err();
        assert!(matches!(e, ForgeError::Invalid(_)), "{what}: {e}");
    };
    bad(&|o| o.hostname = Some("bad host".into()), "hostname");
    bad(&|o| o.user = Some("Root".into()), "user");
    bad(&|o| o.user = None, "password without user");
    bad(&|o| o.ssh_authorized_keys = Some("not a key".into()), "key");
    bad(&|o| o.wifi.as_mut().unwrap().ssid = "x".repeat(33), "ssid 33 bytes");
    bad(&|o| o.wifi.as_mut().unwrap().ssid = "è".repeat(17), "ssid 34 bytes");
    bad(&|o| o.wifi.as_mut().unwrap().ssid = String::new(), "empty ssid");
    bad(&|o| o.wifi.as_mut().unwrap().ssid = "a\nb".into(), "ssid control char");
    bad(&|o| o.wifi.as_mut().unwrap().password = "short".into(), "wpa short");
    bad(&|o| o.wifi.as_mut().unwrap().country = String::new(), "country mandatory");
    bad(&|o| o.wifi.as_mut().unwrap().country = "ITA".into(), "country format");
    bad(&|o| o.locale = Some("x".into()), "locale");
    bad(&|o| o.time_zone = Some("../x".into()), "tz");
    bad(&|o| o.keyboard_layout = Some("it;x".into()), "keyboard");
    bad(&|o| o.password = Some("a\u{1}b".into()), "password control char");
    bad(&|o| {
        o.password = None;
        o.ssh = false;
    }, "account without any login");
    bad(&|o| {
        o.ssh_authorized_keys = None;
        o.password = None;
    }, "ssh password login without a password");
    // 32 bytes of UTF-8 is fine; an open network is fine.
    let mut o = full();
    o.wifi.as_mut().unwrap().ssid = "è".repeat(16);
    o.wifi.as_mut().unwrap().password = String::new();
    assert!(validate(&o).is_ok());
}

#[test]
fn shell_quoting_survives_a_real_shell() {
    for v in ["plain", "it's", "$(reboot)", "`id`", "a\"b", "back\\slash", "new\nline", "'", "''", "*?[", "✓ é"] {
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(format!("printf '%s' {}", sh_quote(v))).output().unwrap();
        assert_eq!(String::from_utf8(out.stdout).unwrap(), v, "{v:?}");
    }
}

#[test]
fn yaml_quoting_escapes_what_yaml_needs() {
    assert_eq!(yaml_quote("plain ✓"), "\"plain ✓\"");
    assert_eq!(yaml_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    assert_eq!(yaml_quote("l1\nl2\t\r"), "\"l1\\nl2\\t\\r\"");
    assert_eq!(yaml_quote("\u{1}\u{7f}\u{85}\u{2028}"), "\"\\x01\\x7f\\x85\\u2028\"");
    assert_eq!(yaml_quote("runcmd: [rm]"), "\"runcmd: [rm]\"");
}

#[test]
fn heredoc_delimiters_never_occur_in_the_body() {
    assert_eq!(heredoc_delim("a\nb", "EOF"), "EOF");
    assert_eq!(heredoc_delim("a\nEOF\nEOF1", "EOF"), "EOF2");
}

#[test]
fn golden_firstrun_files() {
    let s = validate(&full()).unwrap();
    let script = firstrun_script(&s, Some(HASH), true);
    golden("firstrun_full.sh", &script);
    // Bookworm's initramfs cuts the script at the first `rm …firstrun.sh` line: it must be the
    // one Imager writes, after everything else.
    let lines: Vec<&str> = script.lines().collect();
    let cut = lines.iter().position(|l| l.contains("rm") && l.ends_with("firstrun.sh")).unwrap();
    assert_eq!(lines[cut], "rm -f /boot/firstrun.sh");
    assert_eq!(&lines[cut + 1..], ["sed -i 's| systemd.run.*||g' /boot/cmdline.txt", "exit 0"]);
    assert!(lines[..cut].contains(&"rm -f /boot/userconf.txt /boot/firmware/userconf.txt"));
    assert!(!script.contains("Tr0ub4dor"), "the plain password never appears");

    // Minimal: only a hostname.
    let s = validate(&RpiOptions { hostname: Some("h".into()), ..Default::default() }).unwrap();
    golden("firstrun_hostname.sh", &firstrun_script(&s, None, false));
    // Password SSH, open hidden Wi-Fi with a raw PSK.
    let mut o = full();
    o.ssh_authorized_keys = None;
    o.wifi = Some(Wifi { ssid: "Lab".into(), password: "0123456789abcdef".repeat(4), country: "GB".into(), hidden: false });
    let s = validate(&o).unwrap();
    golden("firstrun_password_ssh.sh", &firstrun_script(&s, Some(HASH), true));
    assert_eq!(userconf_txt("mario", HASH), "mario:$6$LumenDiskTest$golden\n");
}

#[test]
fn firstrun_script_passes_sh_syntax_check() {
    let s = validate(&full()).unwrap();
    let dir = std::env::temp_dir().join(format!("ld-rpi-sh-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("firstrun.sh");
    std::fs::write(&p, firstrun_script(&s, Some(HASH), true)).unwrap();
    let out = std::process::Command::new("/bin/sh").arg("-n").arg(&p).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cmdline_stays_one_line_and_is_idempotent() {
    let orig = "console=serial0,115200 console=tty1 root=PARTUUID=9d05ddda-02 rootfstype=ext4 fsck.repair=yes rootwait quiet init=/usr/lib/raspberrypi-sys-mods/firstboot\r\n";
    let a = cmdline_firstrun(orig, Some("IT"));
    assert_eq!(
        a,
        "console=serial0,115200 console=tty1 root=PARTUUID=9d05ddda-02 rootfstype=ext4 fsck.repair=yes rootwait quiet init=/usr/lib/raspberrypi-sys-mods/firstboot cfg80211.ieee80211_regdom=IT systemd.run=/boot/firstrun.sh systemd.run_success_action=reboot systemd.unit=kernel-command-line.target\n"
    );
    assert_eq!(a.lines().count(), 1);
    assert_eq!(cmdline_firstrun(&a, Some("IT")), a, "applying twice changes nothing");
    assert!(!cmdline_firstrun(&a, None).contains("regdom"));
    // What firstrun.sh's cleanup leaves: the original + regdom.
    let after = a.split(" systemd.run").next().unwrap();
    assert!(after.ends_with("cfg80211.ieee80211_regdom=IT"));

    let c = cmdline_cloud("console=tty1 root=PARTUUID=27b94958-02 rootwait resize\n", Some("DE"), "lumendisk-1");
    assert_eq!(c, "console=tty1 root=PARTUUID=27b94958-02 rootwait resize cfg80211.ieee80211_regdom=DE ds=nocloud;i=lumendisk-1\n");
    assert_eq!(cmdline_cloud(&c, Some("DE"), "lumendisk-2"), c.replace("lumendisk-1", "lumendisk-2"));
}

const TRIXIE_META: &str = "# This is the meta-data configuration file for cloud-init.\n#\n# https://cloudinit.readthedocs.io/\n\ndsmode: local\n\n# Warning: changing this will cause cloud-init...\ninstance_id: rpios-image\n";
const UBUNTU_USER_DATA: &str = "#cloud-config\n\n# This is the user-data configuration file for cloud-init.\n#\n# https://cloudinit.readthedocs.io/\n\n# On first boot, set the (default) ubuntu user's password to \"ubuntu\" and\n# expire user passwords\nchpasswd:\n  expire: true\n  users:\n  - name: ubuntu\n    password: ubuntu\n    type: text\n\n## Set the system's hostname.\n#hostname: ubuntu\n\n# Enable password authentication with the SSH daemon\nssh_pwauth: false\n\n## Install additional packages on first boot\n#packages:\n#- avahi-daemon\n";

#[test]
fn meta_data_keeps_everything_but_the_instance_id() {
    let m = meta_data(Some(TRIXIE_META), "lumendisk-ab");
    assert!(m.contains("dsmode: local\n") && m.contains("# Warning"));
    assert!(!m.contains("instance_id") && !m.contains("rpios-image"));
    assert!(m.ends_with("instance-id: lumendisk-ab\n"));
    assert_eq!(meta_data(None, "x"), "dsmode: local\ninstance-id: x\n");
    assert_eq!(meta_data(Some(&m), "y").matches("instance-id").count(), 1);
}

#[test]
fn user_data_is_merged_into_what_the_image_has() {
    let s = validate(&full()).unwrap();
    let (ud, notes) = user_data(&s, Some(HASH), "ubuntu", Some(UBUNTU_USER_DATA));
    golden("user-data_ubuntu_merged.yaml", &ud);
    assert!(ud.starts_with("#cloud-config\n"));
    assert!(!ud.contains("chpasswd") && !ud.contains("password: ubuntu"), "the stock password / expiry is gone");
    assert_eq!(ud.matches("ssh_pwauth:").count(), 1);
    assert!(ud.contains("ssh_pwauth: false"), "keys given: password login off");
    assert!(ud.contains("#hostname: ubuntu"), "comments kept");
    assert!(notes.iter().any(|n| n.contains("chpasswd")) && notes.iter().any(|n| n.contains("ssh_pwauth")));

    // Trixie's stock user-data is only comments.
    let stock = "#cloud-config\n\n# This is the user-data configuration file for cloud-init.\n#hostname: raspberrypi\n";
    let (ud, notes) = user_data(&s, Some(HASH), "raspios", Some(stock));
    golden("user-data_raspios_full.yaml", &ud);
    assert!(notes.is_empty(), "{notes:?}");
    assert!(ud.contains("  name: \"mario\"") && ud.contains(&format!("  passwd: \"{HASH}\"")) && ud.contains("lock_passwd: false"));

    // Lists are extended, in their own indentation; other keys stay.
    let custom = "#cloud-config\nruncmd:\n- echo hi\n- [ systemctl, enable, --now, ssh ]\npackages: [vim]\nwrite_files:\n  - path: /etc/x\n    content: |\n      hello: world\n";
    let (ud, notes) = user_data(&s, Some(HASH), "raspios", Some(custom));
    assert!(ud.contains("runcmd:\n- echo hi\n- [ systemctl, enable, --now, ssh ]\n"), "{ud}");
    assert_eq!(ud.matches("systemctl, enable").count(), 1, "not duplicated");
    assert!(ud.contains("packages:\n  - avahi-daemon\n") && !ud.contains("vim"), "flow-style list replaced");
    assert!(notes.iter().any(|n| n.contains("packages")));
    assert!(ud.contains("write_files:\n  - path: /etc/x\n    content: |\n      hello: world\n"));

    // Not something we can edit: replaced, and said so.
    for other in ["#!/bin/sh\necho hi\n", "#cloud-config\n{hostname: x}\n", "Content-Type: multipart/mixed\n"] {
        let (ud, notes) = user_data(&s, Some(HASH), "raspios", Some(other));
        assert!(notes.iter().any(|n| n.contains("replaced")), "{other}");
        assert!(!ud.contains("echo hi") && ud.starts_with("#cloud-config\n"));
    }
    // Keys only, no password: the account is locked for passwords.
    let mut o = full();
    o.password = None;
    o.user = None;
    let s = validate(&o).unwrap();
    let (ud, _) = user_data(&s, None, "ubuntu", None);
    assert!(ud.contains("  name: \"ubuntu\"") && ud.contains("lock_passwd: true") && !ud.contains("  passwd: "));
}

#[test]
fn golden_network_config() {
    let s = validate(&full()).unwrap();
    golden("network-config_wifi.yaml", &network_config(s.wifi.as_ref().unwrap()));
    let open = WifiSettings { ssid: "Open".into(), password: String::new(), country: "FR".into(), hidden: false };
    let n = network_config(&open);
    assert!(n.contains("key-management: none") && !n.contains("hidden") && !n.contains("password"));
}

// ───────── in-memory images ─────────

const MIB: u64 = 1 << 20;

/// An MBR disk image like Raspberry Pi OS's: FAT32 boot at 4 MiB, ext4 root after it.
fn image(boot_files: &[(&str, &[u8])], os_release: &str, via_symlink: bool) -> Arc<dyn BlockDev> {
    let (boot_start, boot_len, root_start, root_len) = (4 * MIB, 64 * MIB, 68 * MIB, 32 * MIB);
    let dev: Arc<dyn BlockDev> = Arc::new(MemDev::new((root_start + root_len) as usize));
    let mut mbr = vec![0u8; 512];
    for (i, (t, s, l)) in [(0x0Cu8, boot_start, boot_len), (0x83, root_start, root_len)].iter().enumerate() {
        let e = 446 + i * 16;
        mbr[e + 4] = *t;
        mbr[e + 8..e + 12].copy_from_slice(&((s / 512) as u32).to_le_bytes());
        mbr[e + 12..e + 16].copy_from_slice(&((l / 512) as u32).to_le_bytes());
    }
    mbr[440..444].copy_from_slice(&0x9d05dddau32.to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    dev.write_at(0, &mbr).unwrap();
    let boot: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), boot_start, boot_len).unwrap());
    vfs::format(boot.clone(), FsType::Fat32, "bootfs").unwrap();
    {
        let mut fs = vfs::open_fs(boot, false, MountDefaults::default()).unwrap();
        for (n, d) in boot_files {
            super::super::tree::write_file(fs.as_mut(), &format!("/{n}"), d).unwrap();
        }
        fs.sync().unwrap();
    }
    let root: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), root_start, root_len).unwrap());
    vfs::format(root.clone(), FsType::Ext4, "rootfs").unwrap();
    {
        let mut fs = vfs::open_fs(root, false, MountDefaults::default()).unwrap();
        super::super::tree::write_file(fs.as_mut(), "/usr/lib/os-release", os_release.as_bytes()).unwrap();
        let etc = super::super::tree::mkdir_p(fs.as_mut(), "/etc").unwrap();
        if via_symlink {
            fs.symlink(etc, "os-release", "../usr/lib/os-release", 0, 0).unwrap();
        } else {
            super::super::tree::write_file(fs.as_mut(), "/etc/os-release", os_release.as_bytes()).unwrap();
        }
        fs.sync().unwrap();
    }
    dev
}

fn sha(dev: &Arc<dyn BlockDev>, off: u64, len: u64) -> Vec<u8> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; MIB as usize];
    let mut o = off;
    while o < off + len {
        let n = MIB.min(off + len - o) as usize;
        dev.read_at(o, &mut buf[..n]).unwrap();
        h.update(&buf[..n]);
        o += n as u64;
    }
    h.finalize().to_vec()
}

fn boot_file(dev: &Arc<dyn BlockDev>, name: &str) -> Option<String> {
    let boot: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), 4 * MIB, 64 * MIB).unwrap());
    let mut fs = vfs::open_fs(boot, true, MountDefaults::default()).unwrap();
    read_text(fs.as_mut(), &format!("/{name}")).unwrap()
}

const BOOKWORM: &str = "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"12\"\nVERSION_CODENAME=bookworm\nID=debian\n";
const TRIXIE: &str = "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nVERSION_CODENAME=trixie\nID=debian\n";
const UBUNTU: &str = "PRETTY_NAME=\"Ubuntu 26.04.1 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"26.04\"\nVERSION_CODENAME=resolute\nID=ubuntu\nID_LIKE=debian\n";
const CMDLINE: &[u8] = b"console=serial0,115200 console=tty1 root=PARTUUID=9d05ddda-02 rootfstype=ext4 fsck.repair=yes rootwait\n";
const ISSUE: &[u8] = b"Raspberry Pi reference 2026-10-06\nGenerated using pi-gen\n";

#[test]
fn bookworm_is_customised_with_firstrun_and_only_the_boot_partition_changes() {
    let dev = image(&[("config.txt", b"arm_64bit=1\n"), ("cmdline.txt", CMDLINE), ("issue.txt", ISSUE), ("start4.elf", b"x")], BOOKWORM, true);
    let info = detect(dev.clone()).unwrap().unwrap();
    assert_eq!((info.os.as_str(), info.method.as_str(), info.family.as_str()), ("Raspberry Pi OS (bookworm)", "firstrun", "raspios"));
    assert_eq!((info.boot_partition, info.root_partition), (1, Some(2)));
    let before_root = sha(&dev, 68 * MIB, 32 * MIB);
    let before_head = sha(&dev, 0, 4 * MIB);

    let ctx = JobCtx::detached(false);
    let v = customize(dev.clone(), &full(), &ctx).unwrap();
    assert_eq!(v["method"], "firstrun");
    assert_eq!(v["files"], json!(["firstrun.sh", "userconf.txt", "ssh", "cmdline.txt"]));
    assert_eq!(sha(&dev, 68 * MIB, 32 * MIB), before_root, "root partition untouched");
    assert_eq!(sha(&dev, 0, 4 * MIB), before_head, "partition table untouched");

    let cmd = boot_file(&dev, "cmdline.txt").unwrap();
    assert_eq!(cmd.lines().count(), 1);
    assert!(cmd.trim_end().ends_with("cfg80211.ieee80211_regdom=IT systemd.run=/boot/firstrun.sh systemd.run_success_action=reboot systemd.unit=kernel-command-line.target"));
    let uc = boot_file(&dev, "userconf.txt").unwrap();
    let (u, h) = uc.trim_end().split_once(':').unwrap();
    assert_eq!(u, "mario");
    assert!(crypt::verify("Tr0ub4dor&3", h), "userconf.txt hash verifies");
    assert_eq!(boot_file(&dev, "ssh").as_deref(), Some(""));
    let fr = boot_file(&dev, "firstrun.sh").unwrap();
    assert!(fr.contains(&format!("IMAGER_PASS='{h}'")) && fr.starts_with("#!/bin/sh\n"));
    // The logs never carry the secrets.
    for l in ctx.snapshot(0).logs {
        assert!(!l.msg.contains("Tr0ub4dor") && !l.msg.contains("p@ss") && !l.msg.contains(h), "{}", l.msg);
    }
    assert_eq!(boot_file(&dev, "config.txt").as_deref(), Some("arm_64bit=1\n"), "other files untouched");

    // Customising again replaces, doesn't stack.
    customize(dev.clone(), &full(), &JobCtx::detached(false)).unwrap();
    let cmd2 = boot_file(&dev, "cmdline.txt").unwrap();
    assert_eq!(cmd2.matches("systemd.run=").count(), 1);
    assert_eq!(cmd2.matches("regdom").count(), 1);
}

#[test]
fn trixie_with_cloud_init_files_is_customised_with_cloud_init() {
    let ud = b"#cloud-config\n\n# This is the user-data configuration file for cloud-init.\n#hostname: raspberrypi\n";
    let dev = image(
        &[("config.txt", b"x\n"), ("cmdline.txt", CMDLINE), ("issue.txt", ISSUE), ("bootcode.bin", b"x"), ("user-data", ud), ("meta-data", TRIXIE_META.as_bytes()), ("network-config", b"# comments only\n")],
        TRIXIE,
        false,
    );
    let info = detect(dev.clone()).unwrap().unwrap();
    assert_eq!((info.os.as_str(), info.method.as_str()), ("Raspberry Pi OS (trixie)", "cloud-init"));
    let before_root = sha(&dev, 68 * MIB, 32 * MIB);
    let v = customize(dev.clone(), &full(), &JobCtx::detached(false)).unwrap();
    assert_eq!(v["files"], json!(["user-data", "network-config", "meta-data", "cmdline.txt"]));
    assert_eq!(sha(&dev, 68 * MIB, 32 * MIB), before_root);
    let md = boot_file(&dev, "meta-data").unwrap();
    let id = md.lines().find_map(|l| l.strip_prefix("instance-id: ")).unwrap().to_string();
    assert!(id.starts_with("lumendisk-") && id.len() == 26);
    let cmd = boot_file(&dev, "cmdline.txt").unwrap();
    assert!(cmd.trim_end().ends_with(&format!("cfg80211.ieee80211_regdom=IT ds=nocloud;i={id}")), "{cmd}");
    let ud = boot_file(&dev, "user-data").unwrap();
    let h = ud.lines().find_map(|l| l.strip_prefix("  passwd: \"")).unwrap().trim_end_matches('"');
    assert!(crypt::verify("Tr0ub4dor&3", h));
    assert!(boot_file(&dev, "network-config").unwrap().contains("regulatory-domain: \"IT\""));
    assert!(boot_file(&dev, "firstrun.sh").is_none() && boot_file(&dev, "userconf.txt").is_none());
}

#[test]
fn ubuntu_keeps_its_cmdline_and_other_images_are_refused() {
    let dev = image(&[("config.txt", b"os_prefix=current/\n"), ("start4.elf", b"x"), ("user-data", UBUNTU_USER_DATA.as_bytes()), ("meta-data", b"dsmode: local\ninstance_id: cloud-image\n")], UBUNTU, true);
    let info = detect(dev.clone()).unwrap().unwrap();
    assert_eq!((info.os.as_str(), info.method.as_str(), info.family.as_str()), ("Ubuntu 26.04.1 LTS (Raspberry Pi)", "cloud-init", "ubuntu"));
    let v = customize(dev.clone(), &full(), &JobCtx::detached(false)).unwrap();
    assert_eq!(v["files"], json!(["user-data", "network-config", "meta-data"]));
    assert!(boot_file(&dev, "cmdline.txt").is_none());
    assert!(!boot_file(&dev, "user-data").unwrap().contains("chpasswd"));

    // A Raspberry Pi boot partition of an unknown OS without cloud-init: detected, refused.
    let dev = image(&[("config.txt", b"x\n"), ("cmdline.txt", CMDLINE), ("start.elf", b"x")], "ID=gentoo\nPRETTY_NAME=\"Gentoo\"\n", false);
    let info = detect(dev.clone()).unwrap().unwrap();
    assert_eq!(info.method, "none");
    assert!(matches!(customize(dev, &full(), &JobCtx::detached(false)), Err(ForgeError::Unsupported(_))));

    // Not a Raspberry Pi image at all.
    let dev = image(&[("readme.txt", b"x")], BOOKWORM, false);
    assert!(detect(dev.clone()).unwrap().is_none());
    assert!(matches!(customize(dev, &full(), &JobCtx::detached(false)), Err(ForgeError::Unsupported(_))));
    let blank: Arc<dyn BlockDev> = Arc::new(MemDev::new(1 << 20));
    assert!(detect(blank).unwrap().is_none());
}

#[test]
fn invalid_options_write_nothing() {
    let dev = image(&[("config.txt", b"x\n"), ("cmdline.txt", CMDLINE), ("issue.txt", ISSUE), ("start4.elf", b"x")], BOOKWORM, false);
    let before = sha(&dev, 0, 100 * MIB);
    let mut o = full();
    o.wifi.as_mut().unwrap().country = String::new();
    assert!(matches!(customize(dev.clone(), &o, &JobCtx::detached(false)), Err(ForgeError::Invalid(_))));
    assert_eq!(sha(&dev, 0, 100 * MIB), before);
}

#[test]
fn symlink_loops_and_huge_files_are_handled() {
    let dev = image(&[("config.txt", b"x\n"), ("cmdline.txt", CMDLINE), ("issue.txt", ISSUE), ("start4.elf", b"x")], BOOKWORM, false);
    let root: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(dev.clone(), 68 * MIB, 32 * MIB).unwrap());
    let mut fs = vfs::open_fs(root, false, MountDefaults::default()).unwrap();
    let r = fs.root_ino();
    fs.symlink(r, "a", "b", 0, 0).unwrap();
    fs.symlink(r, "b", "/a", 0, 0).unwrap();
    assert!(resolve(fs.as_mut(), "/a/x").unwrap().is_none(), "loop");
    assert!(read_small(fs.as_mut(), "/etc/os-release", 10).unwrap().is_none(), "over the limit");
    assert!(read_small(fs.as_mut(), "/etc", MAX_TEXT).unwrap().is_none(), "a directory");
    assert_eq!(read_small(fs.as_mut(), "/etc/../etc/./os-release", MAX_TEXT).unwrap().unwrap(), BOOKWORM.as_bytes());
}

#[test]
fn os_release_parsing() {
    let kv = os_release("# c\nPRETTY_NAME=\"Debian \\\"x\\\" 13\"\nID=debian\nVERSION_CODENAME='trixie'\nEMPTY=\nbroken line\n");
    assert_eq!(get(&kv, "PRETTY_NAME"), Some("Debian \"x\" 13"));
    assert_eq!(get(&kv, "VERSION_CODENAME"), Some("trixie"));
    assert_eq!(get(&kv, "EMPTY"), None);
}
