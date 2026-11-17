#!/usr/bin/env bash
# Real-core UI verification (IT + EN): drives a debug build of the app — linked to the real
# liblumendisk_core.a — with real volumes, a TLS LAN share mounted back as a client, the Wi-Fi
# portal, Shadow changes, Ghost Shield hits, a real speed test and a raw attached device, then
# screenshots every screen in Italian and English into dist/ui-screens/verify_<lang>_<screen>.png.
# Everything lives in a temp dir; nothing touches ~/Library/Application Support/LumenDisk.
#
# Create & Format (Forge): the app runs with LUMENDISK_FORGE_VIRTUAL_ONLY=1, so the core refuses
# every disk but attached disk images. A blank image created here is attached as a raw device
# and formatted (GPT + exFAT) through the UI path (selection, confirmation sheet, job polling);
# the result is checked with diskutil and fsck_exfat, then the partition doctor and a checksum
# job run on it / on a hybrid ISO made here. Physical disks are never selected.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

W="$(mktemp -d -t lumendisk-ui)"
OUT="$ROOT/dist/ui-screens"; mkdir -p "$OUT"
DEV=""
FDEV=""
forge_attached() { # (no `hdiutil info | grep -q`: with pipefail an early grep exit fails the pipe)
  [[ "$(hdiutil info 2>/dev/null)" == *"$W/forge/target.img"* ]]
}
detach_forge() { # detach the forge target image (ours), retrying while DiskArbitration holds it
  [ -n "$FDEV" ] || return 0
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    forge_attached || break
    diskutil unmountDisk force "$FDEV" >/dev/null 2>&1 || true
    hdiutil detach "$FDEV" -force >/dev/null 2>&1 || true
    sleep 1
  done
  if forge_attached; then echo "  ✗ could not detach the forge test device $FDEV"; FAIL=1; fi
  FDEV=""
}
cleanup() {
  set +e +o pipefail   # best-effort teardown: "nothing left to unmount" is not an error
  [ -f "$W/control" ] && { echo "quit" > "$W/control"; sleep 4; }
  pkill -f "$W/LumenDisk.app/Contents/MacOS/LumenDisk" 2>/dev/null || true
  mount | grep "$W/mnt" | sed -E 's/.* on (.*) \(nfs.*/\1/' | while read -r m; do /sbin/umount -f "$m" 2>/dev/null || true; done
  if [ -n "$DEV" ]; then
    # The app (or DiskArbitration) may hold the device for a moment after quitting: retry.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      hdiutil info | grep -q "$W/usb.img" || break
      diskutil eject force "$DEV" >/dev/null 2>&1 || hdiutil detach "$DEV" -force >/dev/null 2>&1 || true
      sleep 1
    done
    hdiutil info | grep -q "$W/usb.img" && echo "  ✗ could not detach the test device $DEV"
  fi
  detach_forge
  rm -rf "$W"
}
trap cleanup EXIT

( cd macos && swift build -q 2>&1 | grep -v -E 'deprecated|^$' || true )
mkdir -p "$W/LumenDisk.app/Contents/MacOS" "$W/LumenDisk.app/Contents/Resources" "$W/mnt" "$W/support"
cp macos/.build/debug/LumenDisk "$W/LumenDisk.app/Contents/MacOS/"
cp macos/Resources/Info.plist "$W/LumenDisk.app/Contents/"
cp macos/Resources/AppIcon.icns macos/Resources/VolumeIcon.icns "$W/LumenDisk.app/Contents/Resources/"
codesign --force --sign - "$W/LumenDisk.app" >/dev/null 2>&1 || true

cat > "$W/wid.swift" <<'EOF'
import CoreGraphics
let pid = Int32(CommandLine.arguments[1])!
let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as! [[String: Any]]
var best: (Int, Double) = (0, 0)
for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid && (w[kCGWindowLayer as String] as? Int) == 0 {
  if let b = w[kCGWindowBounds as String] as? [String: Any], let h = b["Height"] as? Double, let wd = b["Width"] as? Double, h * wd > best.1 {
    best = (w[kCGWindowNumber as String] as! Int, h * wd)
  }
}
print(best.0)
EOF
swiftc -O "$W/wid.swift" -o "$W/wid" 2>/dev/null

CLI="$ROOT/build/bin/lumendisk"
( cd "$W" && "$CLI" mkfs ext4 archivio.img 256 Archivio >/dev/null && "$CLI" mkfs ext4 foto.img 128 Foto >/dev/null && "$CLI" mkfs ext4 usb.img 128 CHIAVETTA >/dev/null )
DEV=$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$W/usb.img" | awk '/\/dev\/disk/ {print $1; exit}')

# Forge sources: a small hybrid ISO (ISO 9660 + an MBR in its system area, like isohybrid) with a
# SHA256SUMS next to it, and a 3 GB sparse file (no disk space) for a checksum job long enough to
# be seen in progress.
mkdir -p "$W/forge/src/boot"
echo "LumenDisk live" > "$W/forge/src/README.TXT"
head -c 300000 /dev/urandom > "$W/forge/src/boot/vmlinuz"
hdiutil makehybrid -iso -joliet -default-volume-name LUMENLIVE -o "$W/forge/live.iso" "$W/forge/src" >/dev/null
python3 -I - "$W/forge/live.iso" <<'PY'
import os, struct, sys
p = sys.argv[1]
size = os.path.getsize(p)
with open(p, "r+b") as f:
    mbr = bytearray(512)
    mbr[446:462] = struct.pack("<B3sB3sII", 0x80, b"\x00\x02\x00", 0x17, b"\xfe\xff\xff", 0, size // 512)
    mbr[510:512] = b"\x55\xaa"
    f.write(mbr)
PY
( cd "$W/forge" && shasum -a 256 live.iso > SHA256SUMS )
ISO_SHA=$(cut -d' ' -f1 "$W/forge/SHA256SUMS")
mkfile -n 3g "$W/forge/big.img"

# UI_SCREENSHOT_MODE=1: dark appearance and only disk images listed (no drives of this Mac), for
# the README screenshots in docs/screenshots.
if [ -n "${UI_SCREENSHOT_MODE:-}" ]; then export LUMENDISK_SCREENSHOT_MODE=1 LUMENDISK_DARK=1; fi

FAIL=0
for LANG_CODE in it en; do
  rm -f "$W/ready" "$W/control" "$W/control.ack"
  # A fresh blank 256 MB disk for the Forge jobs (blank: no other screen lists it).
  rm -f "$W/forge/target.img"; mkfile -n 256m "$W/forge/target.img"
  FDEV=$(hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$W/forge/target.img" 2>/dev/null | awk '/\/dev\/disk/ {print $1; exit}')
  FBSD=${FDEV#/dev/}
  [ -n "$FBSD" ] || { echo "  ✗ [$LANG_CODE] could not attach the forge test image"; exit 1; }
  LUMENDISK_FORGE_VIRTUAL_ONLY=1 LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK=1 LUMENDISK_SUPPORT_DIR="$W/support" LUMENDISK_MOUNT_ROOT="$W/mnt" LUMENDISK_LANG="$LANG_CODE" LUMENDISK_NO_BONJOUR=1 \
  LUMENDISK_WINDOW=1180x820 LUMENDISK_READY_FILE="$W/ready" LUMENDISK_CONTROL="$W/control" \
  LUMENDISK_MOUNT="$W/archivio.img|readWrite|0|Archivio;$W/foto.img|shadow|0|Foto" \
  LUMENDISK_SHARE=Archivio LUMENDISK_LAN_LOOPBACK=1 LUMENDISK_PORTAL=Archivio LUMENDISK_SPEEDTEST=Archivio \
    "$W/LumenDisk.app/Contents/MacOS/LumenDisk" > "$W/app-$LANG_CODE.log" 2>&1 &
  PID=$!
  for _ in $(seq 1 240); do [ -f "$W/ready" ] && break; sleep 0.5; done
  [ -f "$W/ready" ] || { echo "  ✗ [$LANG_CODE] app did not finish its launch hooks"; tail -20 "$W/app-$LANG_CODE.log"; exit 1; }
  NMOUNTS=$(mount | grep -c "$W/mnt" || true)
  [ "$NMOUNTS" -ge 3 ] || { echo "  ✗ [$LANG_CODE] expected ≥3 real mounts (local, shadow, LAN client), got $NMOUNTS"; FAIL=1; }
  # real data for Shadow Staging and Ghost Shield
  FOTO=$(mount | grep "$W/mnt" | sed -E 's/.* on (.*) \(nfs.*/\1/' | grep '/Foto' | head -1)
  ARCH=$(mount | grep "$W/mnt" | sed -E 's/.* on (.*) \(nfs.*/\1/' | grep '/Archivio$' | head -1)
  if [ -n "$FOTO" ]; then for i in 1 2 3; do head -c 400000 /dev/urandom > "$FOTO/scatto-$LANG_CODE-$i.jpg"; done; fi
  if [ -n "$ARCH" ]; then mkdir -p "$ARCH/Documenti"; touch "$ARCH/.DS_Store" "$ARCH/Documenti/.DS_Store"; echo x > "$ARCH/Documenti/nota.txt"; xattr -w com.apple.metadata:kMDItemWhereFroms LumenDisk "$ARCH/Documenti/nota.txt"; fi
  N=0
  run() { # seconds, commands…: run commands and wait for their ack
    local t="$1"; shift
    N=$((N+1))
    printf '%s\n' "$@" > "$W/control"
    for _ in $(seq 1 $((t*4))); do grep -q "done $N" "$W/control.ack" 2>/dev/null && return 0; sleep 0.25; done
    echo "  ✗ [$LANG_CODE] timeout: $*"; FAIL=1
  }
  shot() { # name, commands…
    local name="$1"; shift
    N=$((N+1))
    printf '%s\n' "$@" refresh > "$W/control"
    for _ in $(seq 1 60); do grep -q "done $N" "$W/control.ack" 2>/dev/null && break; sleep 0.25; done
    sleep 1.5
    local wid; wid=$("$W/wid" "$PID")
    screencapture -x -o -l"$wid" "$OUT/verify_${LANG_CODE}_${name}.png" 2>/dev/null || true
    local sz; sz=$(stat -f %z "$OUT/verify_${LANG_CODE}_${name}.png" 2>/dev/null || echo 0)
    if [ "$sz" -gt 30000 ]; then echo "  ✓ [$LANG_CODE] $name"; else echo "  ✗ [$LANG_CODE] $name (no screenshot)"; FAIL=1; fi
  }
  shot local "sheet close" "select name:Archivio"
  shot local_health "sheet close" "select name:Archivio" "scroll bottom"
  shot physical "scroll top" "select physical"
  shot lan "select remote0"
  shot shadow "select focus-shadow:Foto"
  shot ghost "select focus-ghost:Archivio"
  shot images "select images"
  shot network "select network"
  shot qr "select name:Archivio" "sheet portal"

  # ── Create & Format ──
  LOG="$W/app-$LANG_CODE.log"
  LABEL="LUMEN$(echo "$LANG_CODE" | tr a-z A-Z)"
  run 90 "sheet close" "select forge:create" "forge disks" "forge target $FBSD create" "forge target-refused"
  grep -q "forge target $FBSD for create: selected=true" "$LOG" && echo "  ✓ [$LANG_CODE] forge: attached test image listed as allowed and selected" \
    || { echo "  ✗ [$LANG_CODE] forge: test image $FBSD not selectable"; FAIL=1; }
  grep -q "selectable=false target-unchanged=true" "$LOG" && echo "  ✓ [$LANG_CODE] forge: a refused disk can't be selected" \
    || { echo "  ✗ [$LANG_CODE] forge: refused disk selection not rejected"; FAIL=1; }
  shot forge_empty "forge clear-image"
  run 30 "forge image $W/forge/live.iso"
  grep -q "forge inspected live.iso raw mode=dd hybrid=true" "$LOG" || { echo "  ✗ [$LANG_CODE] forge: hybrid ISO not recognised"; FAIL=1; }
  shot forge_image "forge drawer open"
  shot forge_drawer_all "forge drawer all" "forge demo-options"
  shot forge_drawer_windows "forge drawer-scroll windows"
  shot forge_drawer_rpi "forge drawer-scroll rpi"
  shot forge_confirm "forge drawer-scroll" "forge reset-options" "forge drawer closed" "forge format gpt exfat $LABEL" "forge prepare"
  run 60 "forge go"
  run 90 "forge wait create 80"
  if grep -q "forge job [0-9]* format state=succeeded" "$LOG"; then
    echo "  ✓ [$LANG_CODE] forge: format job succeeded ($(grep -o 'phases=[a-z,]*' "$LOG" | tail -1))"
  else
    echo "  ✗ [$LANG_CODE] forge: format job did not succeed"; grep "DEBUG: forge" "$LOG" | tail -5; FAIL=1
  fi
  shot forge_result "forge logs open"
  # Check the result outside LumenDisk: GPT with one exFAT partition named $LABEL, fsck clean.
  diskutil unmountDisk "$FDEV" >/dev/null 2>&1 || true
  LIST=$(diskutil list "$FDEV" 2>/dev/null || true)
  if grep -q "GUID_partition_scheme" <<<"$LIST" && grep -q "$LABEL" <<<"$LIST"; then
    echo "  ✓ [$LANG_CODE] forge: diskutil sees GPT + \"$LABEL\" on $FBSD"
  else
    echo "  ✗ [$LANG_CODE] forge: unexpected layout on $FBSD"; echo "$LIST"; FAIL=1
  fi
  if fsck_exfat -n "/dev/r${FBSD}s1" >"$W/fsck-$LANG_CODE.log" 2>&1; then
    echo "  ✓ [$LANG_CODE] forge: fsck_exfat -n clean on ${FBSD}s1"
  else
    echo "  ✗ [$LANG_CODE] forge: fsck_exfat reported problems"; tail -5 "$W/fsck-$LANG_CODE.log"; FAIL=1
  fi
  # Partition doctor (read-only check) on the freshly formatted disk.
  run 60 "forge logs closed" "forge done create" "select forge:doctor" "forge target $FBSD doctor" "forge doctor-check" "forge wait doctor 40"
  grep -q 'forge job [0-9]* doctor state=succeeded.*"healthy":true' "$LOG" && echo "  ✓ [$LANG_CODE] forge: doctor check: healthy GPT" \
    || { echo "  ✗ [$LANG_CODE] forge: doctor check failed"; grep -E "DEBUG: forge (target|doctor|job|wait|confirmed)" "$LOG" | tail -6 | sed 's/^/    ✗ log: /' | cut -c1-400; FAIL=1; }
  shot forge_doctor
  # Checksums: a 3 GB sparse file in progress, then the ISO compared with SHA256SUMS and a pasted value.
  run 20 "forge done doctor" "select forge:hash" "forge hash $W/forge/big.img"
  shot forge_hash_progress
  run 60 "forge wait hash 50" "forge done hash" "forge hash-compare $ISO_SHA" "forge hash $W/forge/live.iso" "forge wait hash 30"
  if grep "forge job [0-9]* hash state=succeeded" "$LOG" | grep "live.iso" | grep -q "\"sha256\":\"$ISO_SHA\"" \
     && grep "forge job [0-9]* hash state=succeeded" "$LOG" | grep "live.iso" | grep -q '"matches":true'; then
    echo "  ✓ [$LANG_CODE] forge: hash job on the ISO matches shasum and SHA256SUMS"
  else
    echo "  ✗ [$LANG_CODE] forge: hash job result unexpected"; grep "hash state" "$LOG" | tail -2 | cut -c1-400; FAIL=1
  fi
  shot forge_hash_result
  shot forge_wipe "forge done hash" "select forge:wipe" "forge target $FBSD wipe" "forge wipe-method dod3"
  # The same screen in light mode (the rest of the run uses the system appearance).
  shot forge_light "select forge:create" "forge image $W/forge/live.iso" "forge drawer open" "appearance light"
  printf 'sheet close\nquit\n' > "$W/control"
  for _ in $(seq 1 60); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done
  kill "$PID" 2>/dev/null || true
  LEFT=$(mount | grep -c "$W/mnt" || true)
  [ "$LEFT" = 0 ] && echo "  ✓ [$LANG_CODE] quit ejected every volume" || { echo "  ✗ [$LANG_CODE] $LEFT mounts left after quit"; FAIL=1; }
  grep -q "launch hooks done" "$W/app-$LANG_CODE.log" || { echo "  ✗ [$LANG_CODE] hooks log missing"; FAIL=1; }
  detach_forge
  forge_attached && { echo "  ✗ [$LANG_CODE] the forge test image is still attached"; FAIL=1; } || echo "  ✓ [$LANG_CODE] forge test image detached"
done
if [ -d "$HOME/Library/Application Support/LumenDisk" ] && [ -n "$(find "$HOME/Library/Application Support/LumenDisk" -newer "$W/wid.swift" -type f ! -name '.verify-sentinel-*' 2>/dev/null)" ]; then
  echo "  ✗ the UI run wrote into ~/Library/Application Support/LumenDisk"; FAIL=1
else
  echo "  ✓ nothing written to ~/Library/Application Support/LumenDisk"
fi
hdiutil info | grep -q "$W/usb.img" && hdiutil detach "$DEV" -force >/dev/null 2>&1
exit $FAIL
