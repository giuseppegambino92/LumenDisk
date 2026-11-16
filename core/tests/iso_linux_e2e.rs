//! ISO reader and Linux ISO mode against independent oracles.
//!
//! (a) The reader vs the Linux kernel: ISO 9660 (Rock Ridge / Joliet / plain), UDF 1.02 – 2.60
//!     images made with xorriso, genisoimage, mkudffs + the kernel's own udf driver, and
//!     LumenDisk's test builders (UDF 2.50/2.60 with a metadata partition, files > 4 GiB), plus
//!     real distribution ISOs: every path, type, size, SHA-256 and link target listed through
//!     a kernel mount in the `lumendisk-toolbox` container must match the reader's.
//! (b) ISO mode end to end with real distribution ISOs booted in QEMU (x86_64, TCG) in the
//!     container, with SeaBIOS and with OVMF, watching the serial console: Alpine virt (SYSLINUX
//!     BIOS path + GRUB EFI), Debian live standard (ISOLINUX 6.04 → SYSLINUX 6.03, GRUB EFI,
//!     live-boot persistence kept across a power cycle, first boot BIOS, second boot UEFI).
//!
//! Run: `cargo test --release -p lumendisk-core --test iso_linux_e2e -- --ignored --test-threads=1`
//! Downloads (verified against pinned SHA-256 values, which were checked against the
//! distributions' signed checksum lists) are cached in `~/.cache/lumendisk-test/iso`.
//!
//! Test-only boot configuration patch (documented, applied to the written image, never by ISO
//! mode itself): `console=ttyS0,115200` on the kernel command lines, a finite menu timeout
//! where the ISO waits forever (Debian: ISOLINUX `timeout 0`, GRUB without `timeout`), so the
//! boot can be watched and driven over the serial console.

mod common;

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use lumendisk_core::block::{open_source, parse_partitions, BlockDev, PartitionDev};
use lumendisk_core::forge::job::JobCtx;
use lumendisk_core::forge::plan::{BootTarget, FlashRequest, LinuxOptions, WriteMode};
use lumendisk_core::forge::target::{open_for_write, TargetSpec};
use lumendisk_core::forge::tree::{self, SourceTree};
use lumendisk_core::iso::{self, View};
use lumendisk_core::vfs::{self, MountDefaults};
use sha2::Digest;

fn virtual_only() {
    std::env::set_var("LUMENDISK_FORGE_VIRTUAL_ONLY", "1");
}

fn hex(d: &[u8]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

// ───────────────────────────── downloads ─────────────────────────────

struct Iso {
    name: &'static str,
    url: &'static str,
    sha256: &'static str,
}

/// Pinned official images (SHA-256 checked against the published, signed lists: Alpine's
/// `.asc` by Natanael Copa 0482D84022F52DF1C4E7CD43293ACD0907D9495A, Debian's SHA256SUMS.sign
/// by the Debian CD signing key DF9B9C49EAA9298432589D76DA87E80D6294BE9B).
const ALPINE_VIRT: Iso = Iso {
    name: "alpine-virt-3.24.4-x86_64.iso",
    url: "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-virt-3.24.4-x86_64.iso",
    sha256: "157d353f8e54252cad2d9812a0d56bb7fa2231fc154a110e1d180d7480ef8b4f",
};
const DEBIAN_LIVE: Iso = Iso {
    name: "debian-live-13.7.0-amd64-standard.iso",
    url: "https://cdimage.debian.org/debian-cd/current-live/amd64/iso-hybrid/debian-live-13.7.0-amd64-standard.iso",
    sha256: "040a44f35186321eb6cdaab52a8a2d06224fe6b77f6fbb9f1861ad89c14e17ec",
};

fn cache_dir() -> PathBuf {
    let d = PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/lumendisk-test/iso");
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sha256_file(p: &Path) -> String {
    let mut f = std::fs::File::open(p).unwrap();
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h).unwrap();
    hex(&h.finalize())
}

/// The cached ISO, downloaded (and verified) if needed.
fn fetch(iso: &Iso) -> PathBuf {
    let path = cache_dir().join(iso.name);
    if path.exists() && sha256_file(&path) == iso.sha256 {
        return path;
    }
    let part = path.with_extension("part");
    eprintln!("downloading {}", iso.url);
    let st = Command::new("curl").args(["-sSfL", "-o"]).arg(&part).arg(iso.url).status().expect("curl");
    assert!(st.success(), "download of {} failed", iso.url);
    let got = sha256_file(&part);
    assert_eq!(got, iso.sha256, "{}: SHA-256 mismatch", iso.name);
    std::fs::rename(&part, &path).unwrap();
    path
}

// ───────────────────────────── (a) reader vs kernel ─────────────────────────────

/// Listing format shared by both sides: "path\tD", "path\tL\ttarget", "path\tF\tsize\tsha256".
const LISTER: &str = r#"
import os, sys, hashlib, stat
root = sys.argv[1]
lower = len(sys.argv) > 2 and sys.argv[2] == "lower"
out = []
for dp, dns, fns in os.walk(root):
    for n in dns + fns:
        p = os.path.join(dp, n)
        rel = "/" + os.path.relpath(p, root)
        if lower:
            rel = rel.lower()
        st = os.lstat(p)
        if stat.S_ISLNK(st.st_mode):
            out.append(rel + "\tL\t" + os.readlink(p))
        elif stat.S_ISDIR(st.st_mode):
            out.append(rel + "\tD")
        else:
            h = hashlib.sha256()
            with open(p, "rb") as f:
                while True:
                    b = f.read(1 << 20)
                    if not b:
                        break
                    h.update(b)
            out.append(rel + "\tF\t" + str(st.st_size) + "\t" + h.hexdigest())
out.sort(key=lambda s: s.encode("utf-8", "surrogateescape"))
sys.stdout.buffer.write(("\n".join(out) + "\n").encode("utf-8", "surrogateescape"))
"#;

fn reader_listing(t: &mut iso::IsoTree, lower: bool) -> Vec<String> {
    let mut out = vec![];
    let mut buf = vec![0u8; 4 << 20];
    for e in t.entries().unwrap() {
        let p = if lower { e.path.to_lowercase() } else { e.path.clone() };
        if let Some(l) = &e.symlink {
            out.push(format!("{p}\tL\t{l}"));
        } else if e.is_dir {
            out.push(format!("{p}\tD"));
        } else {
            let mut h = sha2::Sha256::new();
            let mut off = 0u64;
            while off < e.size {
                let n = t.read(&e.path, off, &mut buf).unwrap_or_else(|err| panic!("{}: {err}", e.path));
                assert!(n > 0, "{}: short read at {off}", e.path);
                h.update(&buf[..n]);
                off += n as u64;
            }
            out.push(format!("{p}\tF\t{}\t{}", e.size, hex(&h.finalize())));
        }
    }
    out.sort();
    out
}

/// List `image` (in `dir`) through an independent implementation in the container: a kernel
/// mount (`fstype` iso9660 / udf, with `mount_opts`), or, for Joliet (the Docker Desktop kernel
/// is built without CONFIG_JOLIET), 7-Zip's ISO handler, which reads the Joliet tree.
fn kernel_listing(dir: &Path, image: &str, fstype: &str, mount_opts: &str, lower: bool) -> Vec<String> {
    std::fs::write(dir.join("lister.py"), LISTER).unwrap();
    let list = format!("PYTHONIOENCODING=utf-8:surrogateescape python3 /w/lister.py {{}} {} > /w/kernel.txt", if lower { "lower" } else { "" });
    let script = if fstype == "joliet" {
        format!(
            "set -e; export LANG=C.UTF-8; rm -rf /tmp/x; 7z x -tiso -o/tmp/x /w/{image} >/tmp/7z.log 2>&1 || {{ cat /tmp/7z.log; exit 1; }}; {}",
            list.replace("{}", "/tmp/x")
        )
    } else {
        format!("set -e; mkdir -p /m; mount -t {fstype} -o ro,loop{mount_opts} /w/{image} /m; {}; timeout 120 umount /m", list.replace("{}", "/m"))
    };
    let (code, out) = common::docker_priv(dir, &script);
    assert_eq!(code, 0, "{image} ({fstype}{mount_opts}): {out}");
    let text = std::fs::read_to_string(dir.join("kernel.txt")).unwrap();
    text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn compare(what: &str, kernel: &[String], ours: &[String]) {
    if kernel == ours {
        eprintln!("  {what}: {} entries identical", ours.len());
        return;
    }
    let k: std::collections::BTreeSet<_> = kernel.iter().collect();
    let o: std::collections::BTreeSet<_> = ours.iter().collect();
    let only_k: Vec<_> = k.difference(&o).take(20).collect();
    let only_o: Vec<_> = o.difference(&k).take(20).collect();
    panic!("{what}: listings differ\n kernel only: {only_k:#?}\n reader only: {only_o:#?}");
}

fn check_view(dir: &Path, image: &str, view: View, fstype: &str, opts: &str) {
    let t0 = Instant::now();
    let dev = open_source(&dir.join(image).to_string_lossy(), true, None).unwrap();
    let mut t = iso::open_view(dev, view).unwrap_or_else(|e| panic!("{image} {view:?}: {e}"));
    let lower = view == View::Iso9660;
    let ours = reader_listing(&mut t, lower);
    let kernel = kernel_listing(dir, image, fstype, opts, lower);
    compare(&format!("{image} {view:?}"), &kernel, &ours);
    eprintln!("    ({:.1}s)", t0.elapsed().as_secs_f64());
}

/// Source tree for the generated images: unicode and long names, spaces, empty files and
/// directories, a directory 11 levels deep (Rock Ridge relocation), symlinks, a hard link and
/// a sparse 5 GiB file with data at both ends.
const MAKE_SRC: &str = r#"
set -e
rm -rf /w/src && mkdir -p /w/src && cd /w/src
mkdir -p "dir with spaces" "ünïcødé ☕" emptydir deep/1/2/3/4/5/6/7/8/9/10
echo hello > readme.txt
head -c 3000001 /dev/urandom > random.bin
: > empty.txt
echo deep > deep/1/2/3/4/5/6/7/8/9/10/deep.txt
echo spaces > "dir with spaces/a file.txt"
echo uni > "ünïcødé ☕/café.txt"
python3 -c 'open("a"*120 + ".txt", "w").write("long name\n")'
ln -s readme.txt link
ln -s /etc/hostname abslink
ln -s "../dir with spaces/a file.txt" deep/rel-link
ln readme.txt hardlink.txt
"#;

const MAKE_HUGE: &str = r#"
set -e
cd /w/src
truncate -s 5G huge.bin
printf HEAD | dd of=huge.bin conv=notrunc status=none
printf TAIL | dd of=huge.bin bs=1 seek=$((5*1024*1024*1024-4)) conv=notrunc status=none
"#;

#[test]
#[ignore]
fn it_reader_matches_the_kernel_on_generated_images() {
    virtual_only();
    let t0 = Instant::now();
    let dir = common::workdir("iso-reader-kernel");
    // xorriso: Rock Ridge + Joliet (long) + ISO level 3 with a 5 GiB multi-extent file.
    let script = format!(
        "{MAKE_SRC}{MAKE_HUGE}
         cd /w
         xorriso -as mkisofs -quiet -input-charset UTF-8 -R -J -joliet-long -iso-level 3 -V XORRISO_RR_JOLIET -o - src | dd of=/w/xorriso.iso bs=1M conv=sparse status=none
         rm -f src/huge.bin
         genisoimage -quiet -input-charset utf-8 -J -joliet-long -V GENISO_JOLIET -o /w/joliet.iso src
         genisoimage -quiet -input-charset utf-8 -R -J -udf -iso-level 3 -V GENISO_UDF -o /w/geniso.iso src
         truncate -s 96M /w/udf201.img
         mkudffs --media-type=hd --udfrev=0x0201 --label=KERNEL_UDF201 /w/udf201.img >/dev/null
         mkdir -p /m && mount -t udf -o loop /w/udf201.img /m
         cp -a src/. /m/
         truncate -s 5G /m/huge.bin
         printf TAIL | dd of=/m/huge.bin bs=1 seek=$((5*1024*1024*1024-4)) conv=notrunc status=none
         timeout 120 umount /m"
    );
    let (code, out) = common::docker_priv(&dir, &script);
    assert_eq!(code, 0, "{out}");
    eprintln!("images generated in {:.1}s", t0.elapsed().as_secs_f64());
    check_view(&dir, "xorriso.iso", View::RockRidge, "iso9660", "");
    check_view(&dir, "xorriso.iso", View::Joliet, "joliet", "");
    check_view(&dir, "xorriso.iso", View::Iso9660, "iso9660", ",norock");
    check_view(&dir, "joliet.iso", View::Joliet, "joliet", "");
    check_view(&dir, "joliet.iso", View::Iso9660, "iso9660", ",norock");
    let dev = open_source(&dir.join("joliet.iso").to_string_lossy(), true, None).unwrap();
    assert_eq!(iso::open(dev).unwrap().view(), View::Joliet, "no Rock Ridge: Joliet names");
    check_view(&dir, "geniso.iso", View::RockRidge, "iso9660", "");
    check_view(&dir, "geniso.iso", View::Joliet, "joliet", "");
    check_view(&dir, "geniso.iso", View::Iso9660, "iso9660", ",norock,nojoliet");
    check_view(&dir, "geniso.iso", View::Udf, "udf", ",utf8");
    check_view(&dir, "udf201.img", View::Udf, "udf", ",utf8");
    // The default view of a bridge disc whose views agree: Rock Ridge (POSIX names, links).
    let dev = open_source(&dir.join("geniso.iso").to_string_lossy(), true, None).unwrap();
    assert_eq!(iso::open(dev).unwrap().view(), View::RockRidge);
    // UDF 2.50 / 2.60 with a metadata partition and a > 4 GiB sparse file (LumenDisk's builder;
    // the kernel is the oracle for both the builder and the reader).
    use lumendisk_core::iso::testimg::udf::{AdKind, UdfBuilder};
    for (rev, bs) in [(0x0250u16, 2048u32), (0x0260, 512)] {
        let mut b = UdfBuilder::new(rev, bs);
        b.label("META_UDF")
            .file("/sources/install.wim", (0..700_000u32).map(|i| (i * 7 % 251) as u8).collect())
            .file("/bootmgr", vec![1, 2, 3])
            .file_with("/efi/boot/bootx64.efi", (0..9000u32).map(|i| i as u8).collect(), AdKind::Long)
            .file("/Ünïcode ☕/x.txt", b"x".to_vec())
            .symlink("/link", "sources/install.wim")
            .sparse_file("/huge.bin", (4u64 << 30) + (3 << 20), vec![(0, b"HEAD".to_vec()), ((4u64 << 30) + (1 << 20), vec![0xAB; 5000])])
            .fragment(4);
        let name = format!("builder-udf-{rev:04x}.img");
        std::fs::write(dir.join(&name), b.build()).unwrap();
        check_view(&dir, &name, View::Udf, "udf", ",utf8");
        // UDF-only image: `open` picks UDF.
        let dev = open_source(&dir.join(&name).to_string_lossy(), true, None).unwrap();
        assert_eq!(iso::open(dev).unwrap().view(), View::Udf);
    }
    // LumenDisk's ISO builder (CE areas, relocation, multi-extent with small extents).
    use lumendisk_core::iso::testimg::iso::IsoBuilder;
    let long = "n".repeat(240);
    IsoBuilder::new("BUILDER")
        .file("/readme.txt", b"hi\n".to_vec())
        .file(&format!("/long/{long}"), b"long".to_vec())
        .file("/multi.bin", (0..50_000u32).map(|i| (i % 253) as u8).collect::<Vec<_>>())
        .symlink("/l", "../x/y")
        .dir("/a/b/c/d/e/f/g/h/i/j")
        .file("/a/b/c/d/e/f/g/h/i/j/k.txt", b"k".to_vec())
        .max_extent(16384)
        .build()
        .write_sparse(&dir.join("builder.iso"))
        .unwrap();
    check_view(&dir, "builder.iso", View::RockRidge, "iso9660", "");
    check_view(&dir, "builder.iso", View::Joliet, "joliet", "");
    check_view(&dir, "builder.iso", View::Iso9660, "iso9660", ",norock");
    eprintln!("total {:.1}s", t0.elapsed().as_secs_f64());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn it_reader_matches_the_kernel_on_real_distribution_isos() {
    virtual_only();
    let dir = common::workdir("iso-reader-real");
    for iso_spec in [ALPINE_VIRT, DEBIAN_LIVE] {
        let src = fetch(&iso_spec);
        // Hard-link the cached ISO into the work dir (same volume), so the container sees it.
        let name = iso_spec.name;
        let _ = std::fs::remove_file(dir.join(name));
        std::fs::hard_link(&src, dir.join(name)).unwrap();
        check_view(&dir, name, View::RockRidge, "iso9660", "");
        check_view(&dir, name, View::Joliet, "joliet", "");
        check_view(&dir, name, View::Iso9660, "iso9660", ",norock");
        let dev = open_source(&src.to_string_lossy(), true, None).unwrap();
        let cat = iso::boot_info(dev.as_ref()).unwrap().unwrap();
        eprintln!("  {name}: El Torito {:?}, hybrid {}", cat.entries.iter().map(|e| (e.platform, e.size)).collect::<Vec<_>>(), iso::is_hybrid(dev.as_ref()));
        assert!(iso::is_hybrid(dev.as_ref()));
        assert!(cat.find(iso::BootPlatform::Bios).is_some() && cat.find(iso::BootPlatform::Efi).is_some());
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ───────────────────────────── (b) ISO mode + QEMU ─────────────────────────────

/// Serial-console driver: runs QEMU, waits for each `expect` regex, optionally sends text,
/// logs everything. Exit 0 when every step matched (and QEMU exited, when asked to).
const DRIVER: &str = r#"
import json, os, re, select, socket, subprocess, sys, time
plan = json.load(open(sys.argv[1]))
log = open(plan["log"], "wb")
sock_path = "/tmp/serial.sock"
cmd = plan["cmd"] + ["-chardev", "socket,id=ser0,path=%s,server=on,wait=on" % sock_path, "-serial", "chardev:ser0"]
qlog = open(plan["log"] + ".qemu", "wb")
p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=qlog, stderr=subprocess.STDOUT)
for _ in range(600):
    if os.path.exists(sock_path):
        break
    time.sleep(0.1)
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(sock_path)
buf = b""
t_start = time.time()
auto = [(re.compile(a.encode()), t) for a, t in plan.get("auto", [])]
def send(text):
    for ch in text:
        s.sendall(ch.encode())
        time.sleep(0.03)
def expect(rx, timeout):
    global buf
    r = re.compile(rx.encode())
    end = time.time() + timeout
    while time.time() < end:
        m = r.search(buf)
        if m:
            buf = buf[m.end():]
            return True
        for a, t in auto:
            am = a.search(buf)
            if am:
                buf = buf[am.end():]
                print("auto %r at t=%.1fs" % (a.pattern, time.time() - t_start), flush=True)
                time.sleep(1.0)
                send(t)
                break
        ready, _, _ = select.select([s], [], [], 1.0)
        if ready:
            d = s.recv(65536)
            if not d:
                return bool(r.search(buf))
            log.write(d); log.flush()
            buf = (buf + d)[-400000:]
    return False
for i, st in enumerate(plan["steps"]):
    t0 = time.time()
    ok = expect(st["expect"], st.get("timeout", 600))
    print("step %d %r: %s after %.1fs (t=%.1fs)" % (i, st["expect"], "ok" if ok else "TIMEOUT", time.time() - t0, time.time() - t_start), flush=True)
    if not ok:
        p.kill()
        sys.exit(1)
    if st.get("send") is not None:
        time.sleep(st.get("delay", 1.0))
        send(st["send"])
if plan.get("wait_exit"):
    deadline = time.time() + plan.get("exit_timeout", 300)
    try:
        while True:
            ready, _, _ = select.select([s], [], [], 1.0)
            if ready:
                d = s.recv(65536)
                if not d:
                    break
                log.write(d); log.flush()
            if p.poll() is not None:
                break
            if time.time() > deadline:
                raise subprocess.TimeoutExpired(cmd, 0)
        p.wait(timeout=plan.get("exit_timeout", 300))
    except subprocess.TimeoutExpired:
        p.kill()
        print("QEMU did not power off", flush=True)
        sys.exit(2)
else:
    p.kill()
print("total %.1fs, QEMU exit %s" % (time.time() - t_start, p.returncode), flush=True)
"#;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Firmware {
    SeaBios,
    Ovmf,
}

struct Step {
    expect: &'static str,
    send: Option<String>,
    timeout: u64,
}

fn step(expect: &'static str, send: Option<&str>, timeout: u64) -> Step {
    Step { expect, send: send.map(str::to_string), timeout }
}

/// Boot `image` (in `dir`) in QEMU and run `steps` over the serial console.
/// `auto`: (regex, text) answered whenever the regex shows up while waiting (login prompts).
fn boot(dir: &Path, image: &str, fw: Firmware, steps: &[Step], auto: &[(&str, &str)], wait_exit: bool, tag: &str) -> f64 {
    let t0 = Instant::now();
    let mut cmd = vec![
        "qemu-system-x86_64".to_string(),
        "-machine".into(),
        "q35".into(),
        "-m".into(),
        "2048".into(),
        "-smp".into(),
        "2".into(),
        "-display".into(),
        "none".into(),
        "-monitor".into(),
        "none".into(),
        "-no-reboot".into(),
        "-drive".into(),
        format!("file=/w/{image},format=raw,if=none,id=usb0,cache=unsafe"),
        "-device".into(),
        "virtio-blk-pci,drive=usb0,bootindex=0".into(),
    ];
    if fw == Firmware::Ovmf {
        cmd.extend(["-drive".into(), "if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd".into(), "-drive".into(), "if=pflash,format=raw,file=/tmp/vars.fd".into()]);
    }
    let plan = serde_json::json!({
        "cmd": cmd,
        "log": format!("/w/serial-{tag}.log"),
        "wait_exit": wait_exit,
        "exit_timeout": 600,
        "auto": auto,
        "steps": steps.iter().map(|s| serde_json::json!({"expect": s.expect, "send": s.send, "timeout": s.timeout})).collect::<Vec<_>>(),
    });
    std::fs::write(dir.join("driver.py"), DRIVER).unwrap();
    std::fs::write(dir.join(format!("plan-{tag}.json")), serde_json::to_vec_pretty(&plan).unwrap()).unwrap();
    let script = format!("cp /usr/share/OVMF/OVMF_VARS_4M.fd /tmp/vars.fd && python3 /w/driver.py /w/plan-{tag}.json");
    let (code, out) = common::docker(dir, &script);
    let secs = t0.elapsed().as_secs_f64();
    eprintln!("  boot {tag} ({fw:?}): {:.1}s\n{}", secs, out.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n"));
    if code != 0 {
        let log = std::fs::read(dir.join(format!("serial-{tag}.log"))).unwrap_or_default();
        let tail = String::from_utf8_lossy(&log[log.len().saturating_sub(6000)..]).to_string();
        panic!("boot {tag} failed (exit {code})\n--- serial tail ---\n{tail}");
    }
    secs
}

/// Write `iso` in ISO mode to a fresh image file of `size` bytes.
fn iso_mode(iso_path: &Path, out: &Path, size: u64, linux: LinuxOptions) -> serde_json::Value {
    let t0 = Instant::now();
    let _ = std::fs::remove_file(out);
    let ctx = JobCtx::detached(false);
    let r = FlashRequest {
        source: iso_path.to_string_lossy().into_owned(),
        target: TargetSpec::File { path: out.into(), size: Some(size) },
        mode: WriteMode::Iso,
        verify: true,
        expect_hash: None,
        windows: None,
        linux: Some(linux),
        rpi: None,
        eject: false,
    };
    let target = open_for_write(&r.target, &ctx).unwrap();
    let src = open_source(&iso_path.to_string_lossy(), true, None).unwrap();
    let res = lumendisk_core::forge::linux::write(&r, src, &target, &ctx).unwrap_or_else(|e| panic!("ISO mode: {e}"));
    eprintln!("  ISO mode {} → {} in {:.1}s:\n{}", iso_path.file_name().unwrap().to_string_lossy(), out.display(), t0.elapsed().as_secs_f64(), serde_json::to_string_pretty(&res).unwrap());
    res
}

/// The test-only patch (see the module documentation): on the written FAT partition, add
/// `console=ttyS0,115200` to every kernel command line and replace infinite menu timeouts.
fn test_console_patch(image: &Path) {
    let disk: Arc<dyn BlockDev> = open_source(&image.to_string_lossy(), false, None).unwrap();
    let parts = parse_partitions(disk.as_ref()).unwrap();
    let p: Arc<dyn BlockDev> = Arc::new(PartitionDev::new(disk.clone(), parts[0].start, parts[0].size).unwrap());
    let mut fs = vfs::open_fs(p, false, MountDefaults::default()).unwrap();
    let mut todo = vec![(fs.root_ino(), String::new())];
    let mut files = vec![];
    while let Some((ino, path)) = todo.pop() {
        for d in fs.readdir(ino).unwrap() {
            if d.name == "." || d.name == ".." {
                continue;
            }
            let pth = format!("{path}/{}", d.name);
            match d.kind {
                vfs::FileKind::Dir => todo.push((d.ino, pth)),
                _ if d.name.to_lowercase().ends_with(".cfg") => files.push(pth),
                _ => {}
            }
        }
    }
    for f in files {
        let Some(data) = tree::read_file(fs.as_mut(), &f).unwrap() else { continue };
        let Ok(text) = String::from_utf8(data) else { continue };
        let mut out = String::new();
        for line in text.split_inclusive('\n') {
            let tok = line.split_whitespace().next().unwrap_or("").to_lowercase();
            let body = line.trim_end_matches(['\n', '\r']);
            let nl = &line[body.len()..];
            if (tok == "append" || tok == "linux" || tok == "linuxefi") && !body.contains("console=ttyS0") {
                // Keep "--- quiet"-style tails after the console parameter.
                out.push_str(body);
                out.push_str(" console=ttyS0,115200");
                out.push_str(nl);
            } else if tok == "timeout" && body.split_whitespace().nth(1) == Some("0") {
                out.push_str("timeout 30");
                out.push_str(nl);
            } else {
                out.push_str(line);
            }
        }
        // GRUB menus without a timeout wait forever.
        if f.to_lowercase().ends_with("grub/grub.cfg") && !out.contains("set timeout") {
            out = format!("set timeout=3\n{out}");
        }
        if out != text {
            tree::write_file(fs.as_mut(), &f, out.as_bytes()).unwrap();
            eprintln!("  test patch: {f}");
        }
    }
    fs.sync().unwrap();
}

#[test]
#[ignore]
fn it_iso_mode_alpine_boots_with_seabios_and_ovmf() {
    virtual_only();
    let dir = common::workdir("iso-mode-alpine");
    let iso_path = fetch(&ALPINE_VIRT);
    for (boot_target, fws) in [(BootTarget::Both, vec![Firmware::SeaBios, Firmware::Ovmf]), (BootTarget::Uefi, vec![Firmware::Ovmf])] {
        let img = dir.join("alpine.img");
        let res = iso_mode(&iso_path, &img, 512 << 20, LinuxOptions { boot: Some(boot_target), ..Default::default() });
        assert_eq!(res["scheme"], if boot_target == BootTarget::Uefi { "gpt" } else { "mbr" });
        test_console_patch(&img);
        for fw in fws {
            let tag = format!("alpine-{boot_target:?}-{fw:?}").to_lowercase();
            let mut steps = vec![];
            if fw == Firmware::SeaBios {
                steps.push(step("SYSLINUX 6.03", None, 300));
            }
            steps.push(step("login:", Some("root\n"), 900));
            steps.push(step("localhost:~#", Some("cat /proc/cmdline; mount | grep -c media; poweroff\n"), 300));
            boot(&dir, "alpine.img", fw, &steps, &[], true, &tag);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn it_iso_mode_debian_live_persistence_survives_a_power_cycle() {
    virtual_only();
    let dir = common::workdir("iso-mode-debian");
    let iso_path = fetch(&DEBIAN_LIVE);
    let img = dir.join("debian.img");
    let res = iso_mode(&iso_path, &img, 4 << 30, LinuxOptions { persistence_size: Some(1 << 30), ..Default::default() });
    assert_eq!(res["persistence"]["label"], "persistence");
    assert!(res["bootloaders"].to_string().contains("SYSLINUX 6.03"));
    test_console_patch(&img);
    let marker = format!("lumendisk-{}", std::process::id());
    // Log in on the serial console if live-config doesn't log in by itself (user / live).
    let auto = [("debian login: $", "user\n"), ("Password: $", "live\n")];
    // First boot: BIOS (SYSLINUX 6.03). Write a file in the home directory, power off.
    let steps = vec![
        step("SYSLINUX 6.03", None, 300),
        step("user@debian:~\\$ $", Some(&format!("grep -o persistence /proc/cmdline; echo {marker} > ~/persist.txt; sync; sudo poweroff\n")), 2400),
    ];
    boot(&dir, "debian.img", Firmware::SeaBios, &steps, &auto, true, "debian-bios");
    // Second boot: UEFI (GRUB from the ISO). The file must still be there.
    let found: &'static str = Box::leak(format!("FOUND-{marker}").into_boxed_str());
    let steps = vec![
        step("user@debian:~\\$ $", Some("echo FOUND-$(cat ~/persist.txt); sudo poweroff\n"), 2400),
        step(found, None, 120),
    ];
    boot(&dir, "debian.img", Firmware::Ovmf, &steps, &auto, true, "debian-uefi");
    let _ = std::io::stderr().flush();
    let _ = std::fs::remove_dir_all(&dir);
}
