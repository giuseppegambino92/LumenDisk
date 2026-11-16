//! Real end-to-end tests: volumes are mounted through the loopback NFSv3 server with the real
//! `/sbin/mount_nfs`, exercised with ordinary file APIs (the macOS kernel NFS client), unmounted,
//! and then verified with the upstream Linux tools in Docker.
//!
//!   cargo test -p lumendisk-core --release --test mount_e2e -- --ignored --test-threads=1
mod common;
use common::*;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use lumendisk_core::vfs::testutil::pattern;
use lumendisk_core::volume::{runtime, MountMode, MountRequest, VolumeStatus};

fn setup(name: &str) -> PathBuf {
    if std::env::var_os("NFS_DEBUG").is_some() {
        let _ = tracing_subscriber::fmt().with_env_filter("nfsserve=debug").with_writer(std::io::stderr).try_init();
    }
    setup_in_process("mount_e2e", name)
}

fn req(dir: &Path, img: &str, partition: u32, mode: MountMode) -> MountRequest {
    MountRequest {
        path: dir.join(img).to_string_lossy().into_owned(),
        partition,
        mode,
        ghost_shield: true,
        ghost_rules: None,
        uid_map: None,
        mount_root: Some(mount_root(&dir).to_string_lossy().into_owned()),
        name: None,
        fd: None,
        volume_icon: None,
        media_uuid: None,
        expect_session: None,
    }
}

/// Force-unmount this test's volumes if it panics, so no mount outlives its server.
struct Cleanup;
impl Drop for Cleanup {
    fn drop(&mut self) {
        if std::thread::panicking() {
            unmount_this_tests_volumes();
        }
    }
}

fn mount(r: MountRequest) -> (VolumeStatus, PathBuf) {
    let st = runtime().mount(r).expect("mount failed");
    let mp = PathBuf::from(&st.mountpoint);
    (st, mp)
}

fn unmount(st: &VolumeStatus) {
    unmount_settled(&st.id);
    assert!(!lumendisk_core::nfs_loopback::is_mountpoint(Path::new(&st.mountpoint)));
}

fn sha(data: &[u8]) -> String {
    // tiny FNV-1a 64 is enough to compare contents across tools
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// The same workload for every filesystem, done through the kernel NFS client.
fn workload(mp: &Path, posix: bool) {
    fs::write(mp.join("hello.txt"), "ciao dal Mac\n").unwrap();
    fs::create_dir_all(mp.join("Progetti/Città/2026")).unwrap();
    let big = pattern(24 * 1024 * 1024 + 12345, 42);
    {
        let mut f = fs::File::create(mp.join("Progetti/big.bin")).unwrap();
        for c in big.chunks(1 << 20) {
            f.write_all(c).unwrap();
        }
        f.sync_all().unwrap();
    }
    assert_eq!(fs::read(mp.join("Progetti/big.bin")).unwrap(), big);
    for i in 0..300 {
        fs::write(mp.join(format!("Progetti/Città/2026/nota numero {i:03}.md")), format!("# nota {i}\n")).unwrap();
    }
    for i in (0..300).step_by(3) {
        fs::remove_file(mp.join(format!("Progetti/Città/2026/nota numero {i:03}.md"))).unwrap();
    }
    assert_eq!(fs::read_dir(mp.join("Progetti/Città/2026")).unwrap().count(), 200);
    fs::rename(mp.join("hello.txt"), mp.join("Progetti/saluti.txt")).unwrap();
    fs::rename(mp.join("Progetti/Città"), mp.join("Archivio")).unwrap();
    let mut f = fs::OpenOptions::new().append(true).open(mp.join("Progetti/saluti.txt")).unwrap();
    f.write_all(b"seconda riga\n").unwrap();
    drop(f);
    // Finder-style junk: must stay off the disk.
    fs::write(mp.join(".DS_Store"), b"Bud1 finder").unwrap();
    fs::write(mp.join("Progetti/.DS_Store"), b"Bud1 finder").unwrap();
    let st = std::process::Command::new("/usr/bin/xattr").args(["-w", "com.apple.metadata:kMDItemWhereFroms", "lumendisk"]).arg(mp.join("Progetti/saluti.txt")).status().unwrap();
    if !st.success() {
        let o = std::process::Command::new("/bin/ls").arg("-la@eO").arg(mp.join("Progetti")).arg(&mp).output().unwrap();
        panic!("xattr failed; listing:\n{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    }
    fs::create_dir_all(mp.join(".fseventsd")).unwrap();
    fs::write(mp.join(".fseventsd/fseventsd-uuid"), b"x").unwrap();
    // truncate
    let f = fs::OpenOptions::new().write(true).open(mp.join("Progetti/big.bin")).unwrap();
    f.set_len(10 * 1024 * 1024).unwrap();
    drop(f);
    assert_eq!(fs::metadata(mp.join("Progetti/big.bin")).unwrap().len(), 10 * 1024 * 1024);
    if posix {
        std::os::unix::fs::symlink("Progetti/saluti.txt", mp.join("link")).unwrap();
        assert_eq!(fs::read_to_string(mp.join("link")).unwrap(), "ciao dal Mac\nseconda riga\n");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(mp.join("Progetti/saluti.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    }
    // The Mac user owns everything it sees.
    use std::os::unix::fs::MetadataExt;
    let me = unsafe { libc::getuid() };
    assert_eq!(fs::metadata(mp.join("Progetti/saluti.txt")).unwrap().uid(), me);
}

fn expected_big() -> String {
    let mut b = pattern(24 * 1024 * 1024 + 12345, 42);
    b.truncate(10 * 1024 * 1024);
    sha(&b)
}

/// Reads a file out of an image with the Linux tools; returns its fnv hash.
fn docker_file_hash(dir: &Path, extract_cmd: &str) -> String {
    let (code, out) = docker(dir, &format!("{extract_cmd} > /tmp/x && python3 -c \"import sys;h=0xcbf29ce484222325\nfor b in open('/tmp/x','rb').read(): h=((h^b)*0x100000001b3)&0xFFFFFFFFFFFFFFFF\nprint('%016x'%h)\""));
    assert_eq!(code, 0, "{out}");
    out.trim().lines().last().unwrap().to_string()
}

#[test]
#[ignore]
fn e2e_ext4_real_mkfs() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_ext4");
    make_sparse(&dir.join("disk.img"), 256);
    let (c, o) = docker(&dir, "mkdir -p src/home/pi && echo cfg > src/home/pi/.bashrc && chown -R 1000:1000 src/home && mkfs.ext4 -q -F -L rootfs -d src disk.img");
    assert_eq!(c, 0, "{o}");
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadWrite));
    assert_eq!(st.fs_type, "EXT4");
    assert_eq!(st.uid_map.linux_uid, 1000, "auto-detected Linux owner");
    assert!(st.forced_read_only_reason.is_none());
    workload(&mp, true);
    let st2 = runtime().list().into_iter().find(|v| v.id == st.id).unwrap();
    assert!(st2.ghost.blocked_count >= 4, "{:?}", st2.ghost);
    unmount(&st);
    let (c, o) = docker(&dir, "e2fsck -fn disk.img");
    assert_eq!(c, 0, "{o}");
    let (c, o) = docker(&dir, "exec 2>/dev/null; debugfs -R 'ls -l /' disk.img; debugfs -R 'ls -l /Progetti' disk.img; debugfs -R 'cat /Progetti/saluti.txt' disk.img; debugfs -R 'ls -l /Archivio/2026' disk.img | grep -c nota");
    assert_eq!(c, 0, "{o}");
    assert!(!o.contains(".DS_Store") && !o.contains("._saluti") && !o.contains(".fseventsd"), "macOS junk reached the disk:\n{o}");
    assert!(o.contains("seconda riga"), "{o}");
    assert!(o.contains(" 1000 ") && o.contains("100600"), "owner/permissions not committed as Linux 1000 / 0600:\n{o}");
    assert_eq!(o.trim().lines().last().unwrap().trim(), "200", "{o}");
    assert_eq!(docker_file_hash(&dir, "debugfs -R 'cat /Progetti/big.bin' disk.img 2>/dev/null"), expected_big());
}

fn fat_like(name: &str, mkfs: &str, fsck: &str, expect: &str) {
    let dir = setup(name);
    make_sparse(&dir.join("disk.img"), 256);
    let (c, o) = docker(&dir, &format!("{mkfs} disk.img"));
    assert_eq!(c, 0, "{o}");
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadWrite));
    assert_eq!(st.fs_type, expect);
    assert!(st.forced_read_only_reason.is_none(), "{:?}", st.forced_read_only_reason);
    workload(&mp, false);
    unmount(&st);
    let (c, o) = docker(&dir, fsck);
    assert_eq!(c, 0, "fsck failed:\n{o}");
    // FAT: mtools reads the image directly. exFAT: the Linux kernel's own exfat driver
    // (read-only loop mount in a privileged container).
    let (c, o) = if expect == "EXFAT" {
        docker_priv(&dir, "mkdir -p /m && mount -t exfat -o loop,ro disk.img /m && ls -aR /m && cat /m/Progetti/saluti.txt && ls /m/Archivio/2026 | wc -l")
    } else {
        docker(&dir, "mdir -i disk.img -/ ::/ ; mtype -i disk.img ::/Progetti/saluti.txt; mdir -i disk.img -b ::/Archivio/2026 | wc -l")
    };
    assert_eq!(c, 0, "{o}");
    assert!(!o.to_lowercase().contains("ds_store") && !o.contains("._saluti") && !o.to_lowercase().contains("fseventsd"), "junk reached the disk:\n{o}");
    assert!(o.contains("seconda riga"), "{o}");
    assert!(o.contains("nota numero 299.md"), "{o}");
    assert_eq!(o.trim().lines().last().unwrap().trim(), "200", "{o}");
    let extract = if expect == "EXFAT" { "true" } else { "mtype -i disk.img ::/Progetti/big.bin" };
    if expect == "EXFAT" {
        let (c, o) = docker_priv(&dir, "mkdir -p /m && mount -t exfat -o loop,ro disk.img /m && python3 -c \"import sys;h=0xcbf29ce484222325\nfor b in open('/m/Progetti/big.bin','rb').read(): h=((h^b)*0x100000001b3)&0xFFFFFFFFFFFFFFFF\nprint('%016x'%h)\"");
        assert_eq!(c, 0, "{o}");
        assert_eq!(o.trim().lines().last().unwrap(), expected_big());
    } else {
        assert_eq!(docker_file_hash(&dir, extract), expected_big());
    }
}

#[test]
#[ignore]
fn e2e_fat32_real_mkfs() {
    let _cleanup = Cleanup;
    fat_like("e2e_fat32", "mkfs.fat -F 32 -n BOOTFS", "fsck.fat -n disk.img", "FAT32");
}

#[test]
#[ignore]
fn e2e_exfat_real_mkfs() {
    let _cleanup = Cleanup;
    fat_like("e2e_exfat", "mkfs.exfat -L SDCARD", "fsck.exfat -n disk.img", "EXFAT");
}

#[test]
#[ignore]
fn e2e_own_images_all_filesystems() {
    let _cleanup = Cleanup;
    for fs_name in ["EXT4", "FAT32", "EXFAT", "NTFS"] {
        let dir = setup(&format!("e2e_own_{fs_name}"));
        let img = dir.join("new.img");
        match runtime().create_image(img.to_str().unwrap(), fs_name, 128, "VIRTUALE") {
            Ok(_) => {}
            Err(e) if fs_name == "NTFS" && e.contains("unsupported") => {
                eprintln!("skipping NTFS: {e}");
                continue;
            }
            Err(e) => panic!("{fs_name}: {e}"),
        }
        let (st, mp) = mount(req(&dir, "new.img", 0, MountMode::ReadWrite));
        assert_eq!(st.fs_type, fs_name);
        assert_eq!(st.name, "VIRTUALE");
        fs::write(mp.join("prova.txt"), "funziona").unwrap();
        assert_eq!(fs::read_to_string(mp.join("prova.txt")).unwrap(), "funziona");
        unmount(&st);
    }
}

#[test]
#[ignore]
fn e2e_shadow_mode_rollback_and_commit() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_shadow");
    let img = dir.join("disk.img");
    runtime().create_image(img.to_str().unwrap(), "EXT4", 64, "SHADOW").unwrap();
    {
        let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadWrite));
        fs::write(mp.join("config.txt"), "arm_64bit=1\n").unwrap();
        fs::write(mp.join("obsoleto.log"), "x".repeat(5000)).unwrap();
        unmount(&st);
    }
    let before = sha(&fs::read(&img).unwrap());
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::Shadow));
    let edit = |mp: &Path| {
        let mut f = fs::OpenOptions::new().append(true).open(mp.join("config.txt")).unwrap();
        f.write_all(b"enable_uart=1\n").unwrap();
        drop(f);
        fs::remove_file(mp.join("obsoleto.log")).unwrap();
        fs::create_dir(mp.join("nuova")).unwrap();
        fs::write(mp.join("nuova/ssh"), b"").unwrap();
        fs::write(mp.join("nuova/wpa.conf"), vec![b'w'; 70_000]).unwrap();
    };
    edit(&mp);
    std::thread::sleep(std::time::Duration::from_secs(4)); // let the periodic sync run
    assert_eq!(sha(&fs::read(&img).unwrap()), before, "Shadow mode touched the physical image!");
    let changes = runtime().shadow_changes(&st.id).unwrap();
    let kinds: Vec<(String, String)> = changes.iter().map(|c| (c.path.clone(), c.kind.clone())).collect();
    assert!(kinds.contains(&("/config.txt".into(), "modified".into())), "{kinds:?}");
    assert!(kinds.contains(&("/obsoleto.log".into(), "deleted".into())), "{kinds:?}");
    assert!(kinds.contains(&("/nuova/wpa.conf".into(), "added".into())), "{kinds:?}");
    let status = runtime().list().into_iter().find(|v| v.id == st.id).unwrap();
    let sh = status.shadow.unwrap();
    assert_eq!((sh.added >= 3, sh.modified, sh.deleted), (true, 1, 1));
    // Eject must refuse while changes are pending.
    assert!(runtime().unmount(&st.id, false).is_err());
    // Rollback: everything back to the original state.
    runtime().shadow_rollback(&st.id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1500)); // actimeo=1
    assert_eq!(fs::read_to_string(mp.join("config.txt")).unwrap(), "arm_64bit=1\n");
    assert!(mp.join("obsoleto.log").exists());
    assert!(!mp.join("nuova").exists());
    // Again, then commit.
    edit(&mp);
    let n = runtime().shadow_commit(&st.id).unwrap();
    assert!(n >= 4, "applied {n}");
    assert_ne!(sha(&fs::read(&img).unwrap()), before);
    assert_eq!(fs::read_to_string(mp.join("config.txt")).unwrap(), "arm_64bit=1\nenable_uart=1\n");
    unmount(&st);
    let (c, o) = docker(&dir, "e2fsck -fn disk.img && debugfs -R 'cat /config.txt' disk.img && debugfs -R 'ls -l /nuova' disk.img && debugfs -R 'stat /obsoleto.log' disk.img");
    assert!(o.contains("enable_uart=1") && o.contains("wpa.conf") && o.contains("70000"), "{o}");
    assert!(o.contains("File not found") || o.contains("not found"), "deleted file still on disk: {o}");
    let _ = c;
    let (c, o) = docker(&dir, "e2fsck -fn disk.img");
    assert_eq!(c, 0, "{o}");
}

#[test]
#[ignore]
fn e2e_read_only_mode() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_ro");
    let img = dir.join("disk.img");
    runtime().create_image(img.to_str().unwrap(), "FAT32", 64, "RO").unwrap();
    let before = sha(&fs::read(&img).unwrap());
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadOnly));
    assert!(st.read_only);
    let e = fs::write(mp.join("x.txt"), "no").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EROFS), "{e}");
    assert!(fs::read_dir(&mp).is_ok());
    unmount(&st);
    assert_eq!(sha(&fs::read(&img).unwrap()), before);
}

#[test]
#[ignore]
fn e2e_dirty_volume_doctor() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_dirty");
    make_sparse(&dir.join("disk.img"), 128);
    // FAT32 with the "volume dirty" bit set exactly like Windows leaves it after a crash.
    let (c, o) = docker(&dir, "mkfs.fat -F 32 -n DIRTY disk.img >/dev/null && python3 - <<'EOF'\nimport struct\nf=open('disk.img','r+b');bs=f.read(512)\nres=struct.unpack_from('<H',bs,14)[0]\nf.seek(res*512+4);v=struct.unpack('<I',f.read(4))[0]\nf.seek(res*512+4);f.write(struct.pack('<I',v & ~0x08000000))\nf.seek(65);f.write(bytes([1]))\nEOF\nfsck.fat -n disk.img; true");
    assert!(o.to_lowercase().contains("dirty"), "fixture not dirty: {o} {c}");
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadWrite));
    assert!(st.forced_read_only_reason.is_some(), "dirty volume must mount read-only");
    assert!(matches!(st.dirty, lumendisk_core::vfs::DirtyState::Dirty(_)));
    assert!(fs::write(mp.join("x.txt"), "no").is_err());
    let (report, st2) = runtime().repair_volume(&st.id).unwrap();
    println!("doctor: {report}");
    assert!(st2.forced_read_only_reason.is_none());
    assert_eq!(st2.dirty, lumendisk_core::vfs::DirtyState::Clean);
    let mp2 = PathBuf::from(&st2.mountpoint);
    assert_eq!(mp2, mp, "remounted at the same place");
    fs::write(mp2.join("x.txt"), "ora sì").unwrap();
    unmount(&st2);
    let (c, o) = docker(&dir, "fsck.fat -n disk.img");
    assert_eq!(c, 0, "{o}");
}

#[test]
#[ignore]
fn e2e_raspberry_pi_style_multi_partition_image() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_rpi");
    make_sparse(&dir.join("rpi.img"), 320);
    // MBR: p1 FAT32 "bootfs" 64 MiB at 4 MiB, p2 ext4 "rootfs" for the rest — like Raspberry Pi OS.
    let script = "printf 'label: dos\\nstart=8192, size=131072, type=c\\nstart=139264, type=83\\n' | sfdisk -q rpi.img && \
        dd if=/dev/zero of=boot.part bs=1M count=64 status=none && mkfs.fat -F 32 -n bootfs boot.part >/dev/null && \
        echo 'console=serial0,115200 root=PARTUUID=1234-02 rootwait' > cmdline.txt && mcopy -i boot.part cmdline.txt ::/ && \
        dd if=boot.part of=rpi.img bs=512 seek=8192 conv=notrunc status=none && \
        mkdir -p root/etc root/home/pi && echo raspberrypi > root/etc/hostname && chown -R 1000:1000 root/home/pi && \
        dd if=/dev/zero of=root.part bs=1M count=252 status=none && mkfs.ext4 -q -F -L rootfs -d root root.part && \
        dd if=root.part of=rpi.img bs=512 seek=139264 conv=notrunc status=none && rm -f root.part boot.part";
    let (c, o) = docker(&dir, script);
    assert_eq!(c, 0, "{o}");
    let rep = runtime().inspect_source(dir.join("rpi.img").to_str().unwrap(), None).unwrap();
    let kinds: Vec<(u32, Option<String>, Option<String>)> = rep.partitions.iter().map(|p| (p.part.index, p.fs_type.clone(), p.label.clone())).collect();
    assert_eq!(kinds, vec![(1, Some("FAT32".into()), Some("bootfs".into())), (2, Some("EXT4".into()), Some("rootfs".into()))], "{kinds:?}");
    let (b, bmp) = mount(req(&dir, "rpi.img", 1, MountMode::ReadWrite));
    let (r, rmp) = mount(req(&dir, "rpi.img", 2, MountMode::ReadWrite));
    assert_eq!((b.name.as_str(), r.name.as_str()), ("bootfs", "rootfs"));
    fs::write(bmp.join("ssh"), b"").unwrap();
    let mut cmd = fs::read_to_string(bmp.join("cmdline.txt")).unwrap();
    cmd = cmd.trim().to_string() + " quiet\n";
    fs::write(bmp.join("cmdline.txt"), cmd).unwrap();
    fs::write(rmp.join("etc/hostname"), "lumenpi\n").unwrap();
    fs::write(rmp.join("home/pi/benvenuto.txt"), "ciao\n").unwrap();
    unmount(&b);
    unmount(&r);
    let (c, o) = docker(&dir, "dd if=rpi.img of=b.part bs=512 skip=8192 count=131072 status=none && dd if=rpi.img of=r.part bs=512 skip=139264 status=none && \
        fsck.fat -n b.part && e2fsck -fn r.part && mtype -i b.part ::/cmdline.txt && mdir -i b.part -b ::/ && debugfs -R 'cat /etc/hostname' r.part && debugfs -R 'ls -l /home/pi' r.part");
    assert_eq!(c, 0, "{o}");
    assert!(o.contains("rootwait quiet") && o.contains("::/ssh") && o.contains("lumenpi"), "{o}");
    assert!(o.contains("benvenuto.txt") && o.contains(" 1000 "), "{o}");
}

#[test]
#[ignore]
fn e2e_lock_inspector_and_safe_eject() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_locks");
    let img = dir.join("disk.img");
    runtime().create_image(img.to_str().unwrap(), "EXT4", 64, "LOCKS").unwrap();
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadWrite));
    fs::write(mp.join("aperto.log"), "x\n").unwrap();
    let mut child = std::process::Command::new("/usr/bin/tail").arg("-f").arg(mp.join("aperto.log")).stdout(std::process::Stdio::null()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let holders = lumendisk_core::lock_inspector::inspect(&st.mountpoint);
    let h = holders.iter().find(|h| h.pid == child.id() as i32).expect("tail not detected");
    assert_eq!(h.name, "tail");
    assert!(h.files.iter().any(|f| f.ends_with("aperto.log")), "{h:?}");
    let gone = lumendisk_core::lock_inspector::release(&[h.pid], 2000);
    assert_eq!(gone, vec![child.id() as i32]);
    let _ = child.wait();
    unmount(&st);
}

#[test]
#[ignore]
fn e2e_ntfs_real_mkntfs() {
    let _cleanup = Cleanup;
    let dir = setup("e2e_ntfs");
    make_sparse(&dir.join("disk.img"), 256);
    let (c, o) = docker(&dir, "mkntfs -F -Q -L WINDATA disk.img");
    assert_eq!(c, 0, "{o}");
    let (st, mp) = mount(req(&dir, "disk.img", 0, MountMode::ReadWrite));
    assert_eq!(st.fs_type, "NTFS");
    assert!(st.forced_read_only_reason.is_none(), "{:?}", st.forced_read_only_reason);
    workload(&mp, false);
    unmount(&st);
    let (c, o) = docker(&dir, "ntfsfix -n disk.img && ntfsls -a disk.img && ntfsls -p /Progetti disk.img && ntfscat disk.img /Progetti/saluti.txt && ntfsls -p /Archivio/2026 disk.img | grep -c nota");
    assert_eq!(c, 0, "{o}");
    assert!(!o.contains(".DS_Store") && !o.contains("._saluti") && !o.contains(".fseventsd"), "junk reached the disk:\n{o}");
    assert!(o.contains("seconda riga"), "{o}");
    assert_eq!(o.trim().lines().last().unwrap().trim(), "200", "{o}");
    assert_eq!(docker_file_hash(&dir, "ntfscat disk.img /Progetti/big.bin"), expected_big());
}

/// Diagnostic: run the workload against a volume mounted by another process (LD_MP=<mountpoint>).
#[test]
#[ignore]
fn workload_on_external_mount() {
    let _cleanup = Cleanup;
    let Some(mp) = std::env::var_os("LD_MP") else { return };
    workload(Path::new(&mp), std::env::var_os("LD_POSIX").is_some());
}
