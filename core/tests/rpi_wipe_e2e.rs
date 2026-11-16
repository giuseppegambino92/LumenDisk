//! End-to-end checks of the Raspberry Pi customisation, the SHA-512 crypt and the secure wipe
//! against real images and real tools.
//!
//! ```sh
//! LUMENDISK_FORGE_VIRTUAL_ONLY=1 cargo test --release -p lumendisk-core --test rpi_wipe_e2e -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! - Downloads (once, cached in `~/.cache/lumendisk-test/rpi/`, never in the repository) the
//!   current Raspberry Pi OS Lite arm64 (trixie, cloud-init), the last bookworm-based Lite
//!   (`raspios_oldstable`) and Ubuntu Server for Raspberry Pi, checks their published SHA-256,
//!   decompresses them in Docker, customises an APFS clone of each, and checks the result with
//!   upstream tools: `cloud-init schema` (with netplan's parser for network-config), `bash -n` +
//!   shellcheck, `mkpasswd` / `openssl passwd` for the hashes, `fsck.vfat -n` / `e2fsck -fn`, and
//!   that no byte outside the boot partition changed.
//! - Wipes files and an attached disk image with every method and checks the final contents.
//!
//! Docker images: `lumendisk-toolbox` (scripts/docker/Dockerfile) and `lumendisk-rpi-qa`
//! (tests/rpi/Dockerfile, built here when missing).
//!
//! Every test refuses to run without `LUMENDISK_FORGE_VIRTUAL_ONLY=1`: the only disks touched
//! are images this suite attaches itself (and detaches, even on panic).

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use lumendisk_core::block::{self, BlockDev};
use lumendisk_core::forge::job::JobCtx;
use lumendisk_core::forge::plan::{RpiOptions, Wifi, WipeMethod, WipeRequest};
use lumendisk_core::forge::target::{self, TargetSpec};
use lumendisk_core::forge::{crypt, rpi, wipe, ForgeError};
use sha2::{Digest, Sha256};

const QA_IMAGE: &str = "lumendisk-rpi-qa";

fn require_virtual_only() {
    assert_eq!(
        std::env::var("LUMENDISK_FORGE_VIRTUAL_ONLY").as_deref(),
        Ok("1"),
        "run this suite with LUMENDISK_FORGE_VIRTUAL_ONLY=1 (only attached disk images may be written)"
    );
}

fn cache_dir() -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/lumendisk-test/rpi");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `docker run --rm` of `image` with `mounts` (host, container[, ro]) running `sh -c script`.
fn docker_in(image: &str, mounts: &[(&Path, &str, bool)], script: &str) -> (i32, String) {
    let mut c = Command::new("docker");
    c.args(["run", "--rm"]);
    for (h, ct, ro) in mounts {
        c.arg("-v").arg(format!("{}:{}{}", h.display(), ct, if *ro { ":ro" } else { "" }));
    }
    let out = c.args([image, "sh", "-c", script]).output().expect("docker not available");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

fn ensure_qa_image() {
    let ok = Command::new("docker").args(["image", "inspect", QA_IMAGE]).output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        let ctx = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/rpi");
        let st = Command::new("docker").args(["build", "-t", QA_IMAGE]).arg(&ctx).status().unwrap();
        assert!(st.success(), "docker build {QA_IMAGE}");
    }
}

fn sha256_file(p: &Path) -> String {
    use std::io::Read;
    let mut f = std::fs::File::open(p).unwrap();
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 8 << 20];
    loop {
        let n = f.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    hex(&h.finalize())
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn curl(url: &str, out: &Path) {
    let st = Command::new("curl").args(["-sSfL", "--retry", "3", "-o"]).arg(out).arg(url).status().unwrap();
    assert!(st.success(), "download {url}");
}

fn curl_text(url: &str) -> String {
    let o = Command::new("curl").args(["-sSfL", "--retry", "3", url]).output().unwrap();
    assert!(o.status.success(), "download {url}");
    String::from_utf8_lossy(&o.stdout).into_owned()
}

struct Source {
    name: &'static str,
    /// Directory URL holding `file` and its checksum.
    url: &'static str,
    file: &'static str,
    /// Where the published SHA-256 is: "<file>.sha256" or a SHA256SUMS listing.
    sums: &'static str,
}

const TRIXIE: Source = Source {
    name: "trixie",
    url: "https://downloads.raspberrypi.com/raspios_lite_arm64/images/raspios_lite_arm64-2026-10-06/",
    file: "2026-10-06-raspios-trixie-arm64-lite.img.xz",
    sums: "2026-10-06-raspios-trixie-arm64-lite.img.xz.sha256",
};
const BOOKWORM: Source = Source {
    name: "bookworm",
    url: "https://downloads.raspberrypi.com/raspios_oldstable_lite_arm64/images/raspios_oldstable_lite_arm64-2026-10-06/",
    file: "2026-10-06-raspios-bookworm-arm64-lite.img.xz",
    sums: "2026-10-06-raspios-bookworm-arm64-lite.img.xz.sha256",
};
const UBUNTU: Source = Source {
    name: "ubuntu",
    url: "https://cdimage.ubuntu.com/releases/26.04.1/release/",
    file: "ubuntu-26.04.1-preinstalled-server-arm64+raspi.img.xz",
    sums: "SHA256SUMS",
};

/// The decompressed pristine image (downloaded, checksum-verified and decompressed once).
fn pristine(src: &Source) -> PathBuf {
    let dir = cache_dir();
    let xz = dir.join(src.file);
    let ok_marker = dir.join(format!("{}.sha256-ok", src.file));
    if !ok_marker.exists() {
        let sums = curl_text(&format!("{}{}", src.url, src.sums));
        let want = sums
            .lines()
            .find(|l| l.split_whitespace().nth(1).map(|n| n.trim_start_matches('*')) == Some(src.file))
            .and_then(|l| l.split_whitespace().next())
            .unwrap_or_else(|| panic!("{}: no checksum for {}", src.sums, src.file))
            .to_ascii_lowercase();
        if !xz.exists() {
            let part = dir.join(format!("{}.part", src.file));
            eprintln!("downloading {}{}", src.url, src.file);
            curl(&format!("{}{}", src.url, src.file), &part);
            std::fs::rename(&part, &xz).unwrap();
        }
        let got = sha256_file(&xz);
        assert_eq!(got, want, "{}: SHA-256 differs from the published one", src.file);
        std::fs::write(&ok_marker, &got).unwrap();
    }
    let img = dir.join(src.file.trim_end_matches(".xz"));
    if !img.exists() {
        let name = src.file;
        let (rc, out) = docker_in("lumendisk-toolbox", &[(&dir, "/c", false)], &format!("cd /c && xz -dkc -T0 '{name}' > '{name}.img.part' && mv '{name}.img.part' '{}'", name.trim_end_matches(".xz")));
        assert_eq!(rc, 0, "{out}");
    }
    img
}

/// A private copy of `img` (an APFS clone: instant, no extra space).
fn work_copy(img: &Path, dir: &Path) -> PathBuf {
    let dst = dir.join(img.file_name().unwrap());
    let _ = std::fs::remove_file(&dst);
    let st = Command::new("cp").arg("-c").arg(img).arg(&dst).status().unwrap();
    if !st.success() {
        std::fs::copy(img, &dst).unwrap();
    }
    dst
}

fn hash_range(dev: &dyn BlockDev, off: u64, len: u64) -> String {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 8 << 20];
    let mut o = off;
    while o < off + len {
        let n = ((off + len - o) as usize).min(buf.len());
        dev.read_at(o, &mut buf[..n]).unwrap();
        h.update(&buf[..n]);
        o += n as u64;
    }
    hex(&h.finalize())
}

/// Checks the values cloud-init will read (YAML parsed by PyYAML, as cloud-init does).
const CLOUD_VALUES_PY: &str = r#"
import yaml
md = yaml.safe_load(open("meta-data"))
assert md["instance-id"].startswith("lumendisk-"), md
ud = yaml.safe_load(open("user-data"))
assert ud["hostname"] == "lumen-pi" and ud["user"]["name"] == "mario", ud
assert ud["user"]["passwd"].startswith("$6$") and ud["user"]["lock_passwd"] is False, ud
assert ud["ssh_pwauth"] is False and ud["user"]["ssh_authorized_keys"][0].startswith("ssh-ed25519 "), ud
assert ud["timezone"] == "Europe/Rome" and ud["keyboard"]["layout"] == "it" and ud["locale"] == "it_IT.UTF-8", ud
assert "chpasswd" not in ud, ud
nc = yaml.safe_load(open("network-config"))
w = nc["network"]["wifis"]["wlan0"]
ap = w["access-points"]
assert list(ap) == ['Casa "Rossi" ✓'], ap
a = ap['Casa "Rossi" ✓']
assert a["password"] == "p@ss w'rd\\$(x)" and a["hidden"] is True, a
assert w["regulatory-domain"] == "IT", w
print("VALUES_OK")
"#;

const PASSWORD: &str ="Pässwörd ✓ 'q\" $x";
const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEx1bWVuRGlzayBlMmUgdGVzdCBrZXkgMDAwMDAwMA lumendisk-e2e";

fn options() -> RpiOptions {
    RpiOptions {
        hostname: Some("lumen-pi".into()),
        user: Some("mario".into()),
        password: Some(PASSWORD.into()),
        ssh: true,
        ssh_authorized_keys: Some(format!("{KEY}\n")),
        wifi: Some(Wifi { ssid: "Casa \"Rossi\" ✓".into(), password: "p@ss w'rd\\$(x)".into(), country: "it".into(), hidden: true }),
        locale: Some("it_IT.UTF-8".into()),
        time_zone: Some("Europe/Rome".into()),
        keyboard_layout: Some("it".into()),
    }
}

/// Customise one real image and check it with upstream tools.
fn customise_real(src: &Source, expect_os: &str, expect_method: &str) {
    require_virtual_only();
    ensure_qa_image();
    let img = pristine(src);
    let dir = common::workdir(&format!("rpi-{}", src.name));
    let work = work_copy(&img, &dir);
    let dev = block::open_source(&work.to_string_lossy(), false, None).unwrap();

    let info = rpi::detect(dev.clone()).unwrap().expect("a Raspberry Pi image");
    eprintln!("{}: {:?}", src.name, info);
    assert_eq!(info.os, expect_os);
    assert_eq!(info.method, expect_method);
    let parts = block::parse_partitions(dev.as_ref()).unwrap();
    let boot = parts.iter().find(|p| p.index == info.boot_partition).unwrap().clone();
    let root = parts.iter().find(|p| Some(p.index) == info.root_partition).unwrap().clone();
    let size = dev.size();
    let before_head = hash_range(dev.as_ref(), 0, boot.start);
    let tail_start = boot.start + boot.size;
    let before_tail = hash_range(dev.as_ref(), tail_start, size - tail_start);

    let ctx = JobCtx::detached(true);
    let t0 = Instant::now();
    let v = rpi::customize(dev.clone(), &options(), &ctx).unwrap();
    eprintln!("{}: customised in {:?}: {v}", src.name, t0.elapsed());
    for l in ctx.snapshot(0).logs {
        assert!(!l.msg.contains("Pässwörd") && !l.msg.contains("p@ss"), "secret in a log line: {}", l.msg);
    }
    assert_eq!(hash_range(dev.as_ref(), 0, boot.start), before_head, "bytes before the boot partition changed");
    assert_eq!(hash_range(dev.as_ref(), tail_start, size - tail_start), before_tail, "bytes after the boot partition changed");
    // Detection is stable after customising.
    assert_eq!(rpi::detect(dev.clone()).unwrap().unwrap().method, expect_method);
    drop(dev);

    // Copy the boot files out (mtools) and check them.
    let out = dir.join("boot");
    std::fs::create_dir_all(&out).unwrap();
    let files: Vec<String> = v["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect();
    let fname = work.file_name().unwrap().to_string_lossy().into_owned();
    let copies: String = files.iter().map(|f| format!("mcopy -n -i /w/{fname}@@{} ::{f} /w/boot/{f} && ", boot.start)).collect();
    let script = format!(
        "{copies} mdir -i /w/{fname}@@{bs} -/ :: > /w/boot/listing.txt && \
         dd if=/w/{fname} of=/tmp/boot.img bs=1M iflag=skip_bytes,count_bytes skip={bs} count={bl} status=none && \
         fsck.vfat -n /tmp/boot.img && echo FSCK_VFAT_OK && \
         e2fsck -fn '/w/{fname}?offset={rs}' && echo E2FSCK_OK",
        bs = boot.start,
        bl = boot.size,
        rs = root.start
    );
    let (rc, log) = docker_in("lumendisk-toolbox", &[(&dir, "/w", false)], &script);
    eprintln!("{log}");
    assert_eq!(rc, 0, "fsck / extraction failed:\n{log}");
    assert!(log.contains("FSCK_VFAT_OK") && log.contains("E2FSCK_OK"));
    let listing = std::fs::read_to_string(out.join("listing.txt")).unwrap();
    // A name that fits 8.3 is stored as a short entry with the lower-case flags (no LFN): mdir
    // shows it as "CMDLINE  TXT".
    let lower = listing.to_lowercase();
    for f in &files {
        let short = match f.split_once('.') {
            Some((b, e)) => format!("{b:<8} {e}"),
            None => f.clone(),
        };
        assert!(lower.contains(f.as_str()) || lower.contains(&short), "{f} not listed:\n{listing}");
    }

    let read = |f: &str| std::fs::read_to_string(out.join(f)).unwrap();
    let mut checks = String::from("set -e\ncd /w/boot\n");
    let hash = if expect_method == "firstrun" {
        let cmd = read("cmdline.txt");
        assert_eq!(cmd.trim_end_matches('\n').lines().count(), 1, "cmdline.txt is one line: {cmd:?}");
        assert!(cmd.trim_end().ends_with("cfg80211.ieee80211_regdom=IT systemd.run=/boot/firstrun.sh systemd.run_success_action=reboot systemd.unit=kernel-command-line.target"), "{cmd}");
        assert!(cmd.contains("init=/usr/lib/raspberrypi-sys-mods/firstboot"), "the image's own parameters stay");
        let uc = read("userconf.txt");
        let (u, h) = uc.trim_end().split_once(':').unwrap();
        assert_eq!(u, "mario");
        assert_eq!(read("ssh"), "");
        let fr = read("firstrun.sh");
        assert!(fr.contains(&format!("IMAGER_PASS='{h}'")));
        checks.push_str("bash -n firstrun.sh && echo BASH_N_OK\nshellcheck -s sh -S warning -e SC2034 firstrun.sh && echo SHELLCHECK_OK\n");
        h.to_string()
    } else {
        let ud = read("user-data");
        assert!(ud.starts_with("#cloud-config\n"));
        let h = ud.lines().find_map(|l| l.strip_prefix("  passwd: \"")).unwrap().trim_end_matches('"').to_string();
        let md = read("meta-data");
        let id = md.lines().find_map(|l| l.strip_prefix("instance-id: ")).unwrap();
        if files.iter().any(|f| f == "cmdline.txt") {
            let cmd = read("cmdline.txt");
            assert_eq!(cmd.trim_end_matches('\n').lines().count(), 1);
            assert!(cmd.trim_end().ends_with(&format!("cfg80211.ieee80211_regdom=IT ds=nocloud;i={id}")), "{cmd}");
        }
        std::fs::write(out.join("values.py"), CLOUD_VALUES_PY).unwrap();
        checks.push_str(
            "cloud-init schema -c user-data --annotate && echo UD_OK\n\
             cloud-init schema -c network-config -t network-config --annotate && echo NC_OK\n\
             python3 -I values.py\n",
        );
        h
    };
    // The hash verifies with the system's own tools, for the given (UTF-8) password.
    let salt = hash.split('$').nth(2).unwrap().to_string();
    std::fs::write(out.join("pw.txt"), PASSWORD).unwrap();
    checks.push_str(&format!(
        "test \"$(mkpasswd -m sha-512 -S '{salt}' --stdin < pw.txt)\" = '{hash}' && echo MKPASSWD_OK\n\
         test \"$(openssl passwd -6 -salt '{salt}' -stdin < pw.txt)\" = '{hash}' && echo OPENSSL_OK\n"
    ));
    let (rc, log) = docker_in(QA_IMAGE, &[(&dir, "/w", false)], &checks);
    eprintln!("{log}");
    assert_eq!(rc, 0, "checks failed:\n{log}");
    let tags: &[&str] = if expect_method == "firstrun" { &["BASH_N_OK", "SHELLCHECK_OK", "MKPASSWD_OK", "OPENSSL_OK"] } else { &["UD_OK", "NC_OK", "VALUES_OK", "MKPASSWD_OK", "OPENSSL_OK"] };
    for tag in tags {
        assert!(log.contains(tag), "{tag}\n{log}");
    }
    let _ = std::fs::remove_file(&work);
}

#[test]
#[ignore]
fn raspios_trixie_cloud_init() {
    customise_real(&TRIXIE, "Raspberry Pi OS (trixie)", "cloud-init");
}

#[test]
#[ignore]
fn raspios_bookworm_firstrun() {
    customise_real(&BOOKWORM, "Raspberry Pi OS (bookworm)", "firstrun");
}

#[test]
#[ignore]
fn ubuntu_server_raspi_cloud_init() {
    customise_real(&UBUNTU, "Ubuntu 26.04.1 LTS (Raspberry Pi)", "cloud-init");
}

// ───────────────────────────── QEMU boot ─────────────────────────────

/// Boots an image on QEMU's `raspi3b` (TCG) from its own kernel / DTB / initramfs, with the
/// kernel command line read from the image's cmdline.txt before each boot (the guest edits it
/// between reboots, and QEMU's direct kernel boot doesn't read it). Waits for the login prompt
/// with the customised host name, signs in with the customised account and password, and
/// prints `id` / `hostname` / sshd state. Exit 0 on success; the console goes to console.log.
const QEMU_DRIVER_PY: &str = r#"
import os, re, select, subprocess, sys, time
img, user, pw, host, deadline_s = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4], int(sys.argv[5])
off = int(subprocess.check_output(["sh", "-c", "sfdisk -d %s | awk '/ : start=/{gsub(\",\",\"\");print $4; exit}'" % img]).strip()) * 512
def mtype(name):
    return subprocess.run(["mtype", "-i", "%s@@%d" % (img, off), "::" + name], capture_output=True).stdout
# The Pi 2 B v1.2 device tree (same BCM2837): no Bluetooth holding the PL011, so ttyAMA0 is
# a console (on the 3 B / 3 B+ trees it belongs to the BT serdev and nothing is printed).
for f in ["kernel8.img", "bcm2710-rpi-2-b.dtb", "initramfs8"]:
    open("/tmp/" + f, "wb").write(mtype(f))
log = open("/w/console.log", "ab")
t_end = time.time() + deadline_s
for boot in range(1, 6):
    cmdline = mtype("cmdline.txt").decode().strip()
    words = [w for w in cmdline.split() if not w.startswith("console=")]
    # QEMU's BCM2835 watchdog resets the guest during boot: its driver is left out.
    append = " ".join(["console=ttyAMA0,115200", "initcall_blacklist=bcm2835_wdt_driver_init"] + words)
    print("boot %d: %s" % (boot, append), flush=True)
    args = ["qemu-system-aarch64", "-M", "raspi3b", "-m", "1G", "-smp", "4",
            "-kernel", "/tmp/kernel8.img", "-dtb", "/tmp/bcm2710-rpi-2-b.dtb",
            "-drive", "file=%s,if=sd,format=raw" % img, "-append", append,
            "-serial", "stdio", "-display", "none", "-monitor", "none", "-no-reboot"]
    # Only the firstrun flow needs the initramfs
    # (bookworm's imager_fixup), so it is used only when systemd.run is on the command line.
    if "systemd.run=" in cmdline and len(mtype("initramfs8")) > 0:
        args[args.index("-append"):args.index("-append")] = ["-initrd", "/tmp/initramfs8"]
    p = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
    buf = b""
    def wait(pattern, secs):
        global buf
        end = min(time.time() + secs, t_end)
        rx = re.compile(pattern)
        while time.time() < end:
            m = rx.search(buf)
            if m:
                buf = buf[m.end():]
                return m
            r, _, _ = select.select([p.stdout], [], [], 1)
            if r:
                d = os.read(p.stdout.fileno(), 65536)
                if not d:
                    return None
                log.write(d); log.flush()
                buf += d
        return None
    def send(s):
        p.stdin.write(s.encode()); p.stdin.flush()
    m = wait(rb"([A-Za-z0-9-]+) login: ", deadline_s)
    if m is None:
        p.wait(timeout=30) if p.poll() is not None else None
        if p.poll() is not None:
            print("boot %d: QEMU exited (guest rebooted), %.0f s left" % (boot, t_end - time.time()), flush=True)
            continue
        print("no login prompt in time", flush=True); p.kill(); sys.exit(2)
    name = m.group(1).decode()
    print("boot %d: login prompt of %r" % (boot, name), flush=True)
    if name != host:
        # cloud-init may still be running; give it the time to finish and retry the prompt.
        send("\n")
        if wait((r"%s login: " % re.escape(host)).encode(), 600) is None:
            print("host name never became %r" % host, flush=True); p.kill(); sys.exit(3)
    time.sleep(20)
    send(user + "\n")
    if wait(rb"Password: ", 120) is None:
        print("no password prompt", flush=True); p.kill(); sys.exit(4)
    time.sleep(1)
    send(pw + "\n")
    m = wait(rb"Login incorrect|\$ ", 180)
    if m is None or m.group(0) == b"Login incorrect":
        print("login failed", flush=True); p.kill(); sys.exit(5)
    print("boot %d: signed in as %s" % (boot, user), flush=True)
    # runcmd (enabling sshd) runs in cloud-init's final stage: wait for it to finish.
    send("cloud-init status --wait >/dev/null 2>&1; echo ID=$(id -un):$(id -Gn | tr ' ' ,) HOST=$(hostname) SSH=$(systemctl is-enabled ssh) CI=$(cloud-init status 2>/dev/null | sed -n 's/^status: //p') TZ=$(readlink /etc/localtime | sed 's,.*zoneinfo/,,')\n")
    m = wait(rb"(ID=\S+ HOST=\S+ SSH=\S+ CI=\S* TZ=\S*)\r?\n", 900)
    print("RESULT " + (m.group(1).decode() if m else "none"), flush=True)
    send("sudo -n poweroff || echo no-sudo\n")
    try:
        p.wait(timeout=10)
    except Exception:
        p.kill()
    sys.exit(0 if m else 6)
print("too many reboots", flush=True)
sys.exit(7)
"#;

fn qemu_boot(src: &Source, minutes: u64) {
    require_virtual_only();
    ensure_qa_image();
    let img = pristine(src);
    let dir = common::workdir(&format!("rpi-qemu-{}", src.name));
    let work = work_copy(&img, &dir);
    {
        let dev = block::open_source(&work.to_string_lossy(), false, None).unwrap();
        let mut o = options();
        o.wifi = None; // no Wi-Fi on raspi3b; keeps the network config the image's own
        let v = rpi::customize(dev, &o, &JobCtx::detached(false)).unwrap();
        eprintln!("{}: {v}", src.name);
    }
    // QEMU's SD card wants a power-of-two size.
    let len = std::fs::metadata(&work).unwrap().len();
    std::fs::OpenOptions::new().write(true).open(&work).unwrap().set_len(len.next_power_of_two()).unwrap();
    std::fs::write(dir.join("driver.py"), QEMU_DRIVER_PY).unwrap();
    let fname = work.file_name().unwrap().to_string_lossy().into_owned();
    let t0 = Instant::now();
    let (rc, log) = docker_in(
        QA_IMAGE,
        &[(&dir, "/w", false)],
        &format!("python3 -I /w/driver.py '/w/{fname}' mario \"$(cat /w/pw)\" lumen-pi {}", minutes * 60).replace("\"$(cat /w/pw)\"", &format!("'{}'", PASSWORD.replace('\'', "'\"'\"'"))),
    );
    eprintln!("{log}\nQEMU {} finished in {:?} (console: {})", src.name, t0.elapsed(), dir.join("console.log").display());
    assert_eq!(rc, 0, "boot check failed");
    let result = log.lines().find(|l| l.starts_with("RESULT ")).unwrap();
    assert!(result.contains("ID=mario:") && result.contains("HOST=lumen-pi") && result.contains("SSH=enabled") && result.contains("TZ=Europe/Rome"), "{result}");
    let _ = std::fs::remove_file(&work);
}

/// Optional (slow): Raspberry Pi OS trixie, cloud-init applied on the first boot.
#[test]
#[ignore]
fn qemu_boot_trixie() {
    qemu_boot(&TRIXIE, 40);
}

/// Optional (slow): Raspberry Pi OS bookworm, firstrun.sh across its reboots.
#[test]
#[ignore]
fn qemu_boot_bookworm() {
    qemu_boot(&BOOKWORM, 50);
}

// ───────────────────────────── crypt ─────────────────────────────

#[test]
#[ignore]
fn crypt_matches_openssl_and_mkpasswd() {
    require_virtual_only();
    ensure_qa_image();
    let dir = common::workdir("rpi-crypt");
    let mut cases = vec![];
    let mut seed = [0u8; 64];
    getrandom::getrandom(&mut seed).unwrap();
    let alphabet: Vec<char> = "abcXYZ019 !\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~àèéìòùç€✓日本語🙂".chars().collect();
    for i in 0..48usize {
        let len = 1 + (seed[i % 64] as usize * 7 + i) % 70;
        let pw: String = (0..len).map(|k| alphabet[(seed[(i + k) % 64] as usize + k * 31 + i) % alphabet.len()]).collect();
        let salt = crypt::random_salt(1 + i % 16).unwrap();
        let rounds = match i % 4 {
            0 => None,
            1 => Some(1000),
            2 => Some(10), // clamped to 1000
            _ => Some(77777),
        };
        cases.push((pw, salt, rounds));
    }
    let mut script = String::from("cd /w\n");
    for (i, (pw, salt, rounds)) in cases.iter().enumerate() {
        std::fs::write(dir.join(format!("pw{i}")), pw).unwrap();
        let (r_mk, s_ssl) = match rounds {
            Some(r) => (format!("-R {r}"), format!("rounds={r}${salt}")),
            None => (String::new(), salt.clone()),
        };
        script.push_str(&format!("echo \"$(mkpasswd -m sha-512 {r_mk} -S '{salt}' --stdin < pw{i})\" > mk{i}\n"));
        script.push_str(&format!("echo \"$(openssl passwd -6 -salt '{s_ssl}' -stdin < pw{i})\" > ssl{i}\n"));
    }
    let (rc, log) = docker_in(QA_IMAGE, &[(&dir, "/w", false)], &script);
    assert_eq!(rc, 0, "{log}");
    let mut compared_mk = 0;
    for (i, (pw, salt, rounds)) in cases.iter().enumerate() {
        let ours = crypt::hash_with(pw.as_bytes(), salt, *rounds).unwrap();
        let mk = std::fs::read_to_string(dir.join(format!("mk{i}"))).unwrap().trim().to_string();
        let ssl = std::fs::read_to_string(dir.join(format!("ssl{i}"))).unwrap().trim().to_string();
        // mkpasswd only takes salts of 8 to 16 characters and refuses rounds below 1000 (where
        // glibc and openssl clamp); openssl takes any.
        if salt.len() >= 8 && rounds.map_or(true, |r| r >= 1000) {
            assert_eq!(ours, mk, "mkpasswd, case {i} ({} chars, rounds {rounds:?})", pw.chars().count());
            compared_mk += 1;
        }
        assert_eq!(ours, ssl, "openssl, case {i} ({} chars, salt {} chars, rounds {rounds:?})", pw.chars().count(), salt.len());
        assert!(crypt::verify(pw, &ssl));
    }
    // And a hash we generate with a random salt verifies with them.
    let h = crypt::hash(&cases[0].0).unwrap();
    let salt = h.split('$').nth(2).unwrap();
    let (rc, log) = docker_in(QA_IMAGE, &[(&dir, "/w", false)], &format!("cd /w && mkpasswd -m sha-512 -S '{salt}' --stdin < pw0"));
    assert_eq!(rc, 0);
    assert_eq!(log.trim(), h);
    assert!(compared_mk >= 15, "{compared_mk}");
    eprintln!("crypt: {} cases identical to openssl passwd -6, {compared_mk} of them also to mkpasswd -m sha-512", cases.len());
}

// ───────────────────────────── wipe ─────────────────────────────

const ALL: [WipeMethod; 5] = [WipeMethod::Zeros, WipeMethod::Random, WipeMethod::Dod3, WipeMethod::Dod7, WipeMethod::Gutmann];
const MIB: u64 = 1 << 20;

/// What the target must hold after a wipe with `master` (last pass, partition areas zeroed).
fn check_wiped(data: &[u8], method: WipeMethod, master: &[u8; 32]) {
    let plan = wipe::passes(method, master);
    let last = plan.len() - 1;
    let size = data.len() as u64;
    let mut want = vec![0u8; MIB as usize];
    let mut off = 0u64;
    while off < size {
        let n = MIB.min(size - off) as usize;
        match plan[last] {
            wipe::Pattern::Byte(b) => want[..n].fill(b),
            wipe::Pattern::Triple(p) => {
                for (i, x) in want[..n].iter_mut().enumerate() {
                    *x = p[((off as usize) + i) % 3];
                }
            }
            wipe::Pattern::Random => wipe::keystream(master, last as u32, off, &mut want[..n]),
        }
        let cleared = plan[last] != wipe::Pattern::Byte(0);
        for (i, w) in want[..n].iter_mut().enumerate() {
            let at = off + i as u64;
            if cleared && (at < MIB || at >= size - MIB) {
                *w = 0;
            }
        }
        assert!(data[off as usize..off as usize + n] == want[..n], "{method:?}: wrong content in MiB {}", off / MIB);
        off += n as u64;
    }
}

fn report(label: &str, v: &serde_json::Value) {
    let ver = &v["verification"];
    eprintln!(
        "{label}: {} passes, {} MB written in {} s = {} MB/s; verify {} MB/s",
        v["pass_count"],
        v["bytes_written"].as_u64().unwrap_or(0) / 1_000_000,
        v["secs"],
        v["mb_s"],
        ver["mb_s"]
    );
}

#[test]
#[ignore]
fn wipe_files_with_every_method() {
    require_virtual_only();
    let dir = common::workdir("wipe-files");
    let size = 64 * MIB + 4096 + 7; // not a multiple of the chunk, nor of a sector
    for m in ALL {
        let path = dir.join(format!("{m:?}.img"));
        std::fs::write(&path, vec![0xC3u8; size as usize]).unwrap();
        let mut master = [0u8; 32];
        getrandom::getrandom(&mut master).unwrap();
        let req = WipeRequest { target: TargetSpec::File { path: path.clone(), size: None }, method: m, verify: true };
        let ctx = JobCtx::detached(false);
        let v = wipe::run_seeded(&req, &ctx, master).unwrap();
        report(&format!("file {m:?}"), &v);
        assert_eq!(v["verification"]["ok"], true);
        check_wiped(&std::fs::read(&path).unwrap(), m, &master);
        let _ = std::fs::remove_file(&path);
    }
}

/// Detaches the attached image even when the test panics.
struct Attached(String);
impl Drop for Attached {
    fn drop(&mut self) {
        let _ = target::detach(&self.0);
    }
}

#[test]
#[ignore]
fn wipe_attached_disk_image_with_every_method() {
    require_virtual_only();
    let dir = common::workdir("wipe-disk");
    let size = 96 * MIB;
    for m in ALL {
        let img = dir.join(format!("{m:?}.img"));
        std::fs::write(&img, vec![0x3Cu8; size as usize]).unwrap();
        let bsd = target::attach_image_for_tests(&img).unwrap();
        let guard = Attached(bsd.clone());
        let info = target::disk_info(&bsd).unwrap();
        assert!(info.disk_image && info.allowed, "{info:?}");
        let mut master = [0u8; 32];
        getrandom::getrandom(&mut master).unwrap();
        let req = WipeRequest { target: TargetSpec::Disk { bsd: bsd.clone() }, method: m, verify: true };
        let ctx = JobCtx::detached(false);
        let v = wipe::run_seeded(&req, &ctx, master).unwrap();
        report(&format!("disk image {bsd} {m:?}"), &v);
        assert_eq!(v["target"], format!("/dev/r{bsd}"));
        drop(guard);
        check_wiped(&std::fs::read(&img).unwrap(), m, &master);
        let _ = std::fs::remove_file(&img);
    }
}

#[test]
#[ignore]
fn wipe_cancel_mid_pass() {
    require_virtual_only();
    let dir = common::workdir("wipe-cancel");
    let size = 256 * MIB;
    let img = dir.join("cancel.img");
    std::fs::write(&img, vec![0x3Cu8; size as usize]).unwrap();
    let bsd = target::attach_image_for_tests(&img).unwrap();
    let guard = Attached(bsd.clone());
    let ctx = JobCtx::detached(false);
    let watcher = {
        let ctx = ctx.clone();
        // Cancel once the second pass (0xFF) is under way.
        std::thread::spawn(move || {
            let t0 = Instant::now();
            while ctx.done() < size + 48 * MIB && t0.elapsed().as_secs() < 120 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            ctx.cancel();
        })
    };
    let req = WipeRequest { target: TargetSpec::Disk { bsd: bsd.clone() }, method: WipeMethod::Dod3, verify: true };
    let r = wipe::run(&req, &ctx);
    watcher.join().unwrap();
    assert!(matches!(r, Err(ForgeError::Cancelled)), "{:?}", r.err());
    let done = ctx.done();
    assert!(done > size && done < 2 * size, "stopped during pass 2: {done}");
    drop(guard);
    let data = std::fs::read(&img).unwrap();
    assert!(data[..(48 * MIB) as usize].iter().all(|&b| b == 0xFF), "pass 2 had started");
    assert!(data[(size - MIB) as usize..].iter().all(|&b| b == 0x00), "the end still holds pass 1");
    eprintln!("cancelled after {done} of {} bytes", size * 3);
}

#[test]
#[ignore]
fn keystream_throughput() {
    require_virtual_only();
    let mut buf = vec![0u8; 256 << 20];
    let t0 = Instant::now();
    wipe::keystream(&[1; 32], 0, 0, &mut buf);
    let s = t0.elapsed().as_secs_f64();
    eprintln!("ChaCha20 keystream: {:.0} MB/s", buf.len() as f64 / 1e6 / s);
    assert!(buf.iter().any(|&b| b != 0));
}
