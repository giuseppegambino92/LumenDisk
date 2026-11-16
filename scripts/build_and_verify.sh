#!/usr/bin/env bash
# LumenDisk — build everything, prove it works, install it.
#
#   scripts/build_and_verify.sh             build + unit tests + real mount/e2e tests (Docker)
#   SKIP_E2E=1 scripts/build_and_verify.sh  build + unit tests only (no Docker needed)
#   FULL=1 scripts/build_and_verify.sh      also the long driver suites (ntfs-3g / kernel ntfs3 / vfat / exfat)
#   SKIP_UI=1 scripts/build_and_verify.sh     skip the real-core UI screenshot pass (it opens app windows)
#   INSTALL=1 scripts/build_and_verify.sh     also copy this development build to /Applications and launch it
#   SKIP_FUZZ=1 scripts/build_and_verify.sh   skip the 30 s-per-target libFuzzer pass (runs only if cargo-fuzz + nightly exist)
#   LUMENDISK_CORRUPT_CASES=50000 ...         more mutants in the corrupted-image suite (default 16000)
#
# The bundle built here is ad-hoc signed. macOS hides the system socket table from such builds,
# so they can't verify loopback NFS clients and refuse to mount unless
# LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1 is set (the tests below set it). scripts/release.sh turns
# this bundle into the Developer ID-signed, notarized release, which verifies every connection.
#
# Produces:
#   build/lib/liblumendisk_core.a   universal (arm64 + x86_64) static library
#   build/bin/lumendisk             universal CLI
#   dist/LumenDisk.app              universal app bundle (ad-hoc signed; input of scripts/release.sh)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"
# Everything this script runs is an ad-hoc development build (see the header).
export LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1

bold() { printf '\n\033[1m▸ %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
die()  { printf '  \033[31m✗ %s\033[0m\n' "$*"; exit 1; }

RESULTS=()
record() { RESULTS+=("$1|$2"); }
summary_of() { grep -E '^test result' <<<"$1" | awk '{p+=$4; f+=$6; i+=$8} END {printf "%d passed, %d failed, %d ignored", p, f, i}'; }

# ── test isolation: remember what the real support dir / ~/Volumes looked like ──
SUPPORT="$HOME/Library/Application Support/LumenDisk"
# Sentinels stand in for "pre-existing user files": they must survive the whole run.
SENT_SUPPORT_DIR_NEW=0; [ -d "$SUPPORT" ] || { mkdir -p "$SUPPORT"; SENT_SUPPORT_DIR_NEW=1; }
SENT_VOLUMES_DIR_NEW=0; [ -d "$HOME/Volumes" ] || { mkdir -p "$HOME/Volumes"; SENT_VOLUMES_DIR_NEW=1; }
SENT1="$SUPPORT/.verify-sentinel-$$"; SENT2="$HOME/Volumes/.verify-sentinel-$$"
echo "user data" > "$SENT1"; echo "user data" > "$SENT2"
SNAP="$(mktemp -t lumendisk-snap)"
( [ -d "$SUPPORT" ] && find "$SUPPORT" -mindepth 1 | sort ) > "$SNAP" || true
VOLUMES_EXISTED=0; [ -d "$HOME/Volumes" ] && VOLUMES_EXISTED=1
cleanup_artifacts() {
  # Remove only what this run created; never touch pre-existing user data.
  if [ -d "$SUPPORT" ]; then
    find "$SUPPORT" -mindepth 1 | sort | comm -13 "$SNAP" - | sort -r | while IFS= read -r p; do
      if [ -d "$p" ]; then rmdir "$p" 2>/dev/null || true; else rm -f "$p"; fi
    done
    [ -s "$SNAP" ] || rmdir "$SUPPORT" 2>/dev/null || true
  fi
  if [ "$VOLUMES_EXISTED" = 0 ] && [ -d "$HOME/Volumes" ]; then
    find "$HOME/Volumes" -mindepth 1 -maxdepth 1 -type d -empty -exec rmdir {} \; 2>/dev/null || true
    rmdir "$HOME/Volumes" 2>/dev/null || true
  fi
  rm -f "$SNAP"
}
trap cleanup_artifacts EXIT

bold "Prerequisites"
command -v cargo >/dev/null || die "cargo not found (install rustup)"
command -v swift >/dev/null || die "swift not found (install Xcode)"
rustup target list --installed | grep -q x86_64-apple-darwin || rustup target add x86_64-apple-darwin
rustup target list --installed | grep -q aarch64-apple-darwin || rustup target add aarch64-apple-darwin
ok "cargo $(cargo --version | cut -d' ' -f2), $(swift --version 2>&1 | head -1 | sed 's/.*Swift version \([0-9.]*\).*/Swift \1/')"
HAVE_DOCKER=0
if [ -z "${SKIP_E2E:-}" ] && docker info >/dev/null 2>&1; then
  HAVE_DOCKER=1
  docker build -q -t lumendisk-toolbox scripts/docker >/dev/null
  ok "Docker toolbox image (e2fsprogs, ntfs-3g, dosfstools, exfatprogs, openssh-server) ready"
fi

bold "Rust core (universal static library + CLI)"
for T in aarch64-apple-darwin x86_64-apple-darwin; do
  MACOSX_DEPLOYMENT_TARGET=14.0 cargo build -q --release -p lumendisk-core --target "$T"
done
mkdir -p build/lib build/bin
lipo -create target/aarch64-apple-darwin/release/liblumendisk_core.a target/x86_64-apple-darwin/release/liblumendisk_core.a \
     -output build/lib/liblumendisk_core.a
lipo -create target/aarch64-apple-darwin/release/lumendisk target/x86_64-apple-darwin/release/lumendisk -output build/bin/lumendisk
ok "build/lib/liblumendisk_core.a ($(lipo -archs build/lib/liblumendisk_core.a))"
ok "build/bin/lumendisk CLI ($(lipo -archs build/bin/lumendisk))"
# (nm warns about the LLVM bitcode section of Rust's prebuilt std objects and exits non-zero;
#  the native symbols are still listed, so capture first and grep afterwards.)
SYMS="$(nm -gU -arch arm64 build/lib/liblumendisk_core.a 2>/dev/null || true)"
NSYM=0
for sym in $(grep -oE 'ld_[a-z_]+\(' core/include/lumendisk_ffi.h | tr -d '(' | sort -u); do
  grep -q " _$sym\$" <<<"$SYMS" || die "missing FFI symbol $sym"
  NSYM=$((NSYM+1))
done
ok "all $NSYM C ABI functions from core/include/lumendisk_ffi.h exported"

bold "Unit tests (drivers, WAL, Ghost Shield, Shadow, UID bridge, remote cache, lock inspector)"
OUT=$(cargo test -q --release -p lumendisk-core --lib 2>&1) || { echo "$OUT" | tail -30; die "unit tests failed"; }
ok "$(summary_of "$OUT")"; record "Unit tests" "$(summary_of "$OUT")"

bold "Remote cache race check: conformance + upload test 100× in a row (stops at the first failure)"
# Runs the already-built unit-test binary directly (no cargo start-up per run): ~10 s.
UNIT_BIN=$(cargo test -q --release -p lumendisk-core --lib --no-run --message-format=json 2>/dev/null | grep -o '"executable":"[^"]*"' | tail -1 | cut -d'"' -f4)
[ -x "$UNIT_BIN" ] || die "unit-test binary not found"
for i in $(seq 1 100); do
  OUT=$("$UNIT_BIN" --exact remote::cache::tests::remote_vfs_conformance_and_upload 2>&1) && grep -q '1 passed' <<<"$OUT" \
    || { echo "$OUT" | tail -20; die "remote_vfs_conformance_and_upload failed on run $i of 100"; }
done
ok "100 of 100 runs passed"; record "Remote cache race check" "100/100 runs"

bold "Corrupted images: mutated ext4 / NTFS / FAT / exFAT volumes and partition tables (no panic, hang or runaway allocation)"
OUT=$(cargo test --profile robust -p lumendisk-core --test corrupted_images -- --nocapture 2>&1) || { echo "$OUT" | grep -E '✗|panicked|FAILED|HANG' | head -40; die "corrupted-image suite failed"; }
LINE=$(grep -E '^corrupted images:' <<<"$OUT" | head -1)
NCASES=$(sed -E 's/^corrupted images: ([0-9]+) cases.*/\1/' <<<"$LINE")
NFS=$(sed -E 's/.* ([0-9]+) filesystems.*/\1/' <<<"$LINE")
grep -E '^corrupted images:|^  mutants mounted|fuzz regression inputs|^partition tables:' <<<"$OUT" | sed 's/^ */    · /'
ok "$(summary_of "$OUT")"; record "Corrupted images ($NCASES cases, $NFS filesystems)" "$(summary_of "$OUT")"

if [ -z "${SKIP_FUZZ:-}" ]; then
  bold "Fuzzing (libFuzzer, 30 s per target)"
  if cargo +nightly --version >/dev/null 2>&1 && cargo +nightly fuzz --version >/dev/null 2>&1; then
    OUT=$(FUZZ_CORPUS="$ROOT/target/fuzz-corpus-verify" scripts/fuzz.sh 30 2>&1) || { echo "$OUT" | tail -25; die "fuzzing found a crash (see core/fuzz/artifacts)"; }
    rm -rf "$ROOT/target/fuzz-corpus-verify"
    echo "$OUT" | grep -E 'execs' | sed 's/^ */    · /'
    EXECS=$(grep -Eo '[0-9]+ execs' <<<"$OUT" | awk '{s+=$1} END {print s+0}')
    ok "$EXECS executions, no crash"; record "Fuzzing (30 s × 5 targets)" "$EXECS execs, 0 crashes"
  else
    printf '  (nightly Rust / cargo-fuzz not installed: skipped; see scripts/fuzz.sh)\n'
  fi
fi

bold "LAN share / Wi-Fi portal server (real HTTP)"
OUT=$(cargo test -q --release -p lumendisk-core --test lan_server 2>&1) || { echo "$OUT" | tail -30; die "LAN server tests failed"; }
ok "$(summary_of "$OUT")"; record "LAN server" "$(summary_of "$OUT")"

bold "Swift 6 app (universal)"
# Remove any binary left by an earlier build first, so the one checked below can only come
# from this build; the build's own exit status decides, the output is filtered separately.
BIN="macos/.build/out/Products/Release/LumenDisk"
# (a fresh clone has no macos/.build yet: find would fail and set -e would end the script silently)
[ ! -d macos/.build ] || find macos/.build -path '*Release*' -name LumenDisk -type f -delete
SWIFT_LOG="$(mktemp -t lumendisk-swift)"
SWIFT_RC=0
( cd macos && swift build -q -c release --arch arm64 --arch x86_64 ) > "$SWIFT_LOG" 2>&1 || SWIFT_RC=$?
grep -v -E 'deprecated|^$' "$SWIFT_LOG" || :
rm -f "$SWIFT_LOG"
[ "$SWIFT_RC" = 0 ] || die "swift build failed (exit $SWIFT_RC)"
[ -x "$BIN" ] || BIN="$(find macos/.build -path '*Release*' -name LumenDisk -type f -perm -111 | head -1)"
[ -x "$BIN" ] || die "Swift build did not produce a binary"
[ "$(lipo -archs "$BIN")" = "x86_64 arm64" ] || die "Swift binary is not universal ($(lipo -archs "$BIN"))"
ok "LumenDisk binary ($(lipo -archs "$BIN"))"

bold "Bundling dist/LumenDisk.app"
APP=dist/LumenDisk.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Helpers"
cp "$BIN" "$APP/Contents/MacOS/LumenDisk"
cp macos/Resources/Info.plist "$APP/Contents/Info.plist"
{ [ -f macos/Resources/AppIcon.icns ] && [ -f macos/Resources/VolumeIcon.icns ]; } || ( cd macos/Resources && swift make_icon.swift . >/dev/null )
cp macos/Resources/AppIcon.icns macos/Resources/VolumeIcon.icns "$APP/Contents/Resources/"
for l in macos/Resources/*.lproj; do [ -d "$l" ] && cp -R "$l" "$APP/Contents/Resources/"; done
cp build/bin/lumendisk "$APP/Contents/Helpers/lumendisk"
printf 'APPL????' > "$APP/Contents/PkgInfo"
# Hardened runtime from the start, so scripts/release.sh can notarize this exact bundle.
codesign --force --options runtime --sign - "$APP/Contents/Helpers/lumendisk" >/dev/null 2>&1
codesign --force --options runtime --sign - "$APP" >/dev/null 2>&1 && ok "ad-hoc signed (hardened runtime)"
plutil -lint "$APP/Contents/Info.plist" >/dev/null && ok "Info.plist valid ($(ls -d "$APP"/Contents/Resources/*.lproj | xargs -n1 basename | tr '\n' ' '))"
ok "$APP ($(du -sh "$APP" | cut -f1))"

bold "App launch smoke test (isolated support dir)"
SMOKE_SUPPORT="$(mktemp -d -t lumendisk-smoke)"
LUMENDISK_SUPPORT_DIR="$SMOKE_SUPPORT" "$APP/Contents/MacOS/LumenDisk" >"$SMOKE_SUPPORT/log" 2>&1 &
PID=$!
sleep 6
if kill -0 $PID 2>/dev/null; then
  ok "app running after 6 s (pid $PID)"; record "App launch" "running after 6 s"
  kill $PID; wait $PID 2>/dev/null || true
else
  cat "$SMOKE_SUPPORT/log"; die "app exited during startup"
fi
rm -rf "$SMOKE_SUPPORT"

if [ "$HAVE_DOCKER" = 1 ]; then
  bold "CLI smoke: create ext4 image → mount via loopback NFS → write → unmount → e2fsck"
  W="$ROOT/target/verify"; rm -rf "$W"; mkdir -p "$W/mnt"
  ( cd "$W" && "$ROOT/build/bin/lumendisk" mkfs ext4 smoke.img 64 SMOKE >/dev/null )
  ( cd "$W" && HOME="$W" "$ROOT/build/bin/lumendisk" mount smoke.img --root "$W/mnt" --control c.sock >mount.log 2>&1 & )
  for _ in $(seq 1 50); do [ -S "$W/c.sock" ] && mount | grep -q "$W/mnt/SMOKE" && break; sleep 0.2; done
  MP="$W/mnt/SMOKE"
  mount | grep -q "$MP" || { cat "$W/mount.log"; die "mount failed"; }
  echo "Ciao da LumenDisk" > "$MP/benvenuto.txt"; mkdir -p "$MP/cartella"; cp /usr/share/dict/words "$MP/cartella/"
  touch "$MP/.DS_Store"
  ( cd "$W" && "$ROOT/build/bin/lumendisk" ctl c.sock unmount >/dev/null )
  for _ in $(seq 1 50); do mount | grep -q "$MP" || break; sleep 0.2; done
  docker run --rm -v "$W:/w" lumendisk-toolbox sh -c 'e2fsck -fn /w/smoke.img >/dev/null && debugfs -R "cat /benvenuto.txt" /w/smoke.img 2>/dev/null && debugfs -R "ls /" /w/smoke.img 2>/dev/null' > "$W/check.txt" \
    || die "e2fsck rejected the smoke image"
  grep -q "Ciao da LumenDisk" "$W/check.txt" || die "written data not found on disk"
  ! grep -q DS_Store "$W/check.txt" || die ".DS_Store reached the disk"
  ok "mounted, written through the macOS NFS client, e2fsck clean, no .DS_Store on disk"
  record "CLI smoke" "ext4 mount/write/unmount + e2fsck clean"

  run_suite() { # name, test target, extra args
    local name="$1" target="$2"; shift 2
    bold "$name"
    local out
    out=$(cargo test --release -p lumendisk-core --test "$target" -- --ignored --test-threads=1 "$@" 2>&1) || { echo "$out" | grep -E '^test |panicked|assert|left|right' | tail -40; die "$name failed"; }
    echo "$out" | grep -E '^test ' | sed 's/^/  /'
    echo "$out" | grep -E 'fsck BEFORE|over HTTP Range|served [0-9]+ bytes|offline: slowest|offline write|overhead\)|fsck right after|speed test on|TLS: |after kill -9|killed during the commit|kill -9 with zero delay|byte-granular ops|kept [0-9]+ changes, endpoint|drift: |then SIGKILL right after Fsync' | sed 's/^/    · /' || true
    ok "$(summary_of "$out")"; record "$name" "$(summary_of "$out")"
  }
  run_suite "ext2/3/4 vs real e2fsprogs (mke2fs defaults, htree, journal replay)" ext4_real_tools
  run_suite "Phase 1 end-to-end: ext4 / FAT32 / exFAT / NTFS, Shadow, Ghost, Doctor, Locks, RPi image" mount_e2e
  run_suite "Phase 2 end-to-end: NTFS symlinks, WAL crash recovery, loopback security, HTTP Range image, offline LAN share, SFTP, portal" phase2_e2e --nocapture
  run_suite "SFTP backend vs real OpenSSH (auth, timeouts, reconnect, range source)" sftp_real
  run_suite "Phase 3 end-to-end: ordered WAL 64 MB power loss, Apache WebDAV RFC 4918 + persistent offline queue, TLS LAN share, physical device + speed test" phase3_e2e --nocapture
  run_suite "Persistent Shadow sessions: kill -9 with staged changes and during a commit, resume, e2fsck" shadow_persist_e2e --nocapture
  run_suite "Remote Shadow sessions: SFTP / WebDAV / LAN share, kill -9, restore, commit to the server, drift on server-side edits" remote_shadow_e2e --nocapture
  run_suite "Persistent Shadow on an attached block device (/dev/diskNs1, /dev/rdiskN): kill -9, resume, commit, e2fsck; raw-node I/O" device_shadow_e2e --nocapture
  run_suite "Zero loss window: SIGKILL right after mkdir / create / rename / unlink / close / fsync, Shadow and read/write, ext4 and FAT32, fsck" durability_e2e --nocapture

  if [ -n "${FULL:-}" ]; then
    bold "Long driver suites (ntfs-3g / kernel ntfs3 / vfat / exfat / fsck_msdos / fsck_exfat)"
    OUT=$(cargo test -q --release -p lumendisk-core --lib -- --ignored --test-threads=1 --skip it_perf 2>&1) || { echo "$OUT" | tail -40; die "driver integration suites failed"; }
    ok "$(summary_of "$OUT")"; record "Driver suites" "$(summary_of "$OUT")"
  fi
  mount | grep -q 'lumendisk-' && die "a test left a LumenDisk mount behind" || ok "no stray mounts left behind"
  rm -rf "$ROOT/target/tmp/it" "$ROOT/target/verify" && ok "test images removed"
else
  printf '\n  (Docker not available or SKIP_E2E set: real mount / e2e verification skipped)\n'
fi

if [ -z "${SKIP_UI:-}" ]; then
  bold "Real-core UI verification (IT + EN): every screen driven with real volumes, TLS share, portal, speed test"
  OUT=$(scripts/ui_verify.sh 2>&1) || { echo "$OUT" | grep -E '✗|error' | head -20; die "UI verification failed"; }
  echo "$OUT" | grep -E '✓|✗' | sed 's/^  /  /'
  NSHOTS=$(ls dist/ui-screens/verify_*.png 2>/dev/null | wc -l | tr -d ' ')
  ok "$NSHOTS screenshots in dist/ui-screens/ (verify_it_*.png, verify_en_*.png)"
  record "UI (real core, IT + EN)" "$NSHOTS screens verified"
fi

bold "Clean teardown"
cleanup_artifacts; trap - EXIT
[ -f "$SENT1" ] && [ -f "$SENT2" ] || die "pre-existing user files were touched by the teardown"
ok "pre-existing files in ~/Library/Application Support/LumenDisk and ~/Volumes survived the teardown"
rm -f "$SENT1" "$SENT2"
[ "$SENT_SUPPORT_DIR_NEW" = 1 ] && rmdir "$SUPPORT" 2>/dev/null || true
[ "$SENT_VOLUMES_DIR_NEW" = 1 ] && rmdir "$HOME/Volumes" 2>/dev/null || true
[ -d "$SUPPORT" ] && ok "pre-existing data in ~/Library/Application Support/LumenDisk left untouched" || ok "no test artifacts in ~/Library/Application Support/LumenDisk"
[ -d "$HOME/Volumes" ] && [ -n "$(ls -A "$HOME/Volumes" 2>/dev/null)" ] && ok "~/Volumes holds only pre-existing items" || ok "no leftovers in ~/Volumes"

if [ -n "${INSTALL:-}" ]; then
  bold "Install the development build to /Applications and launch (needs LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1 to mount)"
  DEST=/Applications/LumenDisk.app
  if [ -d "$DEST" ]; then
    BID=$(/usr/libexec/PlistBuddy -c 'Print CFBundleIdentifier' "$DEST/Contents/Info.plist" 2>/dev/null || true)
    [ "$BID" = "com.lumendisk.app" ] || die "$DEST exists and is not LumenDisk ($BID) — not touching it"
    osascript -e 'tell application id "com.lumendisk.app" to quit' >/dev/null 2>&1 || true
    sleep 1
    rm -rf "$DEST"
  fi
  ditto "$APP" "$DEST"
  ok "installed $DEST"
  open "$DEST" && ok "launched"
  record "Install" "$DEST (launched)"
fi

bold "Summary"
for r in "${RESULTS[@]}"; do printf '  %-90s %s\n' "${r%%|*}" "${r#*|}"; done
printf '\n  App: %s\n  CLI: %s\n' "$ROOT/dist/LumenDisk.app" "$ROOT/build/bin/lumendisk"
