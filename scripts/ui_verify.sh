#!/usr/bin/env bash
# Real-core UI verification (IT + EN): drives a debug build of the app — linked to the real
# liblumendisk_core.a — with real volumes, a TLS LAN share mounted back as a client, the Wi-Fi
# portal, Shadow changes, Ghost Shield hits, a real speed test and a raw attached device, then
# screenshots every screen in Italian and English into dist/ui-screens/verify_<lang>_<screen>.png.
# Everything lives in a temp dir; nothing touches ~/Library/Application Support/LumenDisk.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

W="$(mktemp -d -t lumendisk-ui)"
OUT="$ROOT/dist/ui-screens"; mkdir -p "$OUT"
DEV=""
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

FAIL=0
for LANG_CODE in it en; do
  rm -f "$W/ready" "$W/control" "$W/control.ack"
  LUMENDISK_SUPPORT_DIR="$W/support" LUMENDISK_MOUNT_ROOT="$W/mnt" LUMENDISK_LANG="$LANG_CODE" LUMENDISK_NO_BONJOUR=1 \
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
  printf 'sheet close\nquit\n' > "$W/control"
  for _ in $(seq 1 60); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done
  kill "$PID" 2>/dev/null || true
  LEFT=$(mount | grep -c "$W/mnt" || true)
  [ "$LEFT" = 0 ] && echo "  ✓ [$LANG_CODE] quit ejected every volume" || { echo "  ✗ [$LANG_CODE] $LEFT mounts left after quit"; FAIL=1; }
  grep -q "launch hooks done" "$W/app-$LANG_CODE.log" || { echo "  ✗ [$LANG_CODE] hooks log missing"; FAIL=1; }
done
if [ -d "$HOME/Library/Application Support/LumenDisk" ] && [ -n "$(find "$HOME/Library/Application Support/LumenDisk" -newer "$W/wid.swift" -type f ! -name '.verify-sentinel-*' 2>/dev/null)" ]; then
  echo "  ✗ the UI run wrote into ~/Library/Application Support/LumenDisk"; FAIL=1
else
  echo "  ✓ nothing written to ~/Library/Application Support/LumenDisk"
fi
hdiutil info | grep -q "$W/usb.img" && hdiutil detach "$DEV" -force >/dev/null 2>&1
exit $FAIL
