#!/usr/bin/env bash
# Signed + notarized release artifacts from dist/LumenDisk.app (run scripts/build_and_verify.sh first).
#
# Signing and notarization go through the Apple account logged into Xcode: `xcodebuild -exportArchive`
# with method "developer-id" signs with the team's cloud-managed Developer ID certificate and uploads
# to the notary service, then `-exportNotarizedApp` returns the stapled app. No local certificate,
# app-specific password or API key is needed.
#
#   scripts/release.sh                  → dist/release/{LumenDisk-<v>-{universal,arm64,x86_64}.dmg,
#                                          LumenDisk-<v>-universal.zip, lumendisk-cli-<v>-macos-universal.tar.gz,
#                                          SHA256SUMS.txt}
#   TEAM_ID=XXXXXXXXXX scripts/release.sh   to sign for another team
#   INSTALL=1 scripts/release.sh            also install the notarized universal app to /Applications
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TEAM_ID="${TEAM_ID:-62XX6A3AR9}"
SRC=dist/LumenDisk.app
OUT=dist/release
[ -d "$SRC" ] || { echo "✗ $SRC missing: run scripts/build_and_verify.sh first"; exit 1; }
[ -x "$SRC/Contents/Helpers/lumendisk" ] || { echo "✗ $SRC has no Contents/Helpers/lumendisk: rebuild it"; exit 1; }
PB=/usr/libexec/PlistBuddy
V=$($PB -c 'Print CFBundleShortVersionString' "$SRC/Contents/Info.plist")
BUILD=$($PB -c 'Print CFBundleVersion' "$SRC/Contents/Info.plist")
BUNDLE_ID=$($PB -c 'Print CFBundleIdentifier' "$SRC/Contents/Info.plist")
W="$(mktemp -d -t lumendisk-release)"
cleanup() {
  local rc=$?
  mount | grep -q "$W/mounttest/mnt" && /sbin/umount -f "$W/mounttest/mnt/RELCHECK" 2>/dev/null
  if [ "$rc" = 0 ]; then rm -rf "$W"; else echo "  (work files kept for inspection in $W)"; fi
}
trap cleanup EXIT
ok() { printf '  ✓ %s\n' "$*"; }
die() { printf '  ✗ %s\n' "$*"; exit 1; }
VARIANTS="universal arm64 x86_64"

cat > "$W/export.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
 <key>method</key><string>developer-id</string>
 <key>teamID</key><string>$TEAM_ID</string>
 <key>signingStyle</key><string>automatic</string>
 <key>destination</key><string>upload</string>
</dict></plist>
EOF

# 1. One xcarchive per variant (thin variants are cut from the universal bundle), uploaded for notarization.
echo "▸ LumenDisk $V ($BUILD): signing with Developer ID (team $TEAM_ID) and uploading to Apple"
for VAR in $VARIANTS; do
  A="$W/$VAR.xcarchive"; APP="$A/Products/Applications/LumenDisk.app"
  mkdir -p "$A/Products/Applications"; ditto "$SRC" "$APP"
  if [ "$VAR" = universal ]; then ARCHS="arm64 x86_64"; else
    ARCHS="$VAR"
    for b in "$APP/Contents/MacOS/LumenDisk" "$APP/Contents/Helpers/lumendisk"; do
      lipo -thin "$VAR" "$b" -output "$b.thin" && mv "$b.thin" "$b" && chmod +x "$b"
    done
    # Without this a thin bundle would be byte-identical to that slice of the universal one, so
    # both would share a cdhash. The notary service looks tickets up by cdhash, and the universal
    # app could then be stapled with the thin variant's ticket, which lacks the other slice.
    # Gatekeeper rejects such an app.
    $PB -c "Add :LSArchitecturePriority array" -c "Add :LSArchitecturePriority:0 string $VAR" "$APP/Contents/Info.plist"
  fi
  codesign --force --options runtime --sign - "$APP/Contents/Helpers/lumendisk" 2>/dev/null
  codesign --force --options runtime --sign - "$APP" 2>/dev/null
  ARCH_XML=$(for a in $ARCHS; do printf '<string>%s</string>' "$a"; done)
  cat > "$A/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>ApplicationProperties</key><dict>
 <key>ApplicationPath</key><string>Applications/LumenDisk.app</string>
 <key>Architectures</key><array>$ARCH_XML</array>
 <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
 <key>CFBundleShortVersionString</key><string>$V</string>
 <key>CFBundleVersion</key><string>$BUILD</string>
 <key>SigningIdentity</key><string>Apple Development</string>
 <key>Team</key><string>$TEAM_ID</string>
</dict>
<key>ArchiveVersion</key><integer>2</integer>
<key>CreationDate</key><date>$(date -u +%Y-%m-%dT%H:%M:%SZ)</date>
<key>Name</key><string>LumenDisk</string>
<key>SchemeName</key><string>LumenDisk</string>
</dict></plist>
EOF
  # Apple's services drop connections now and then: retry the upload a few times.
  for try in 1 2 3 4; do
    rm -rf "$W/$VAR-upload"
    if xcodebuild -exportArchive -archivePath "$A" -exportOptionsPlist "$W/export.plist" -exportPath "$W/$VAR-upload" \
         -allowProvisioningUpdates > "$W/$VAR-upload.log" 2>&1 && grep -q 'EXPORT SUCCEEDED' "$W/$VAR-upload.log"; then
      break
    fi
    [ "$try" = 4 ] && { tail -20 "$W/$VAR-upload.log"; die "$VAR: upload failed"; }
    echo "    · $VAR: upload failed ($(grep -m1 -o 'error: .*' "$W/$VAR-upload.log" || echo unknown error)), retrying in 30 s"
    sleep 30
  done
  ok "$VAR ($ARCHS) signed and uploaded"
done

# 2. Wait for Apple's verdict and fetch the stapled apps.
echo "▸ Waiting for notarization"
for VAR in $VARIANTS; do
  A="$W/$VAR.xcarchive"
  for i in $(seq 1 90); do
    if xcodebuild -exportNotarizedApp -archivePath "$A" -exportPath "$W/$VAR" > "$W/$VAR-notary.log" 2>&1 \
       && [ -d "$W/$VAR/LumenDisk.app" ]; then break; fi
    LOG=$(ls "$A"/Submissions/*/notarization-log.json 2>/dev/null | head -1 || true)
    if [ -n "$LOG" ] && grep -q '"status" *: *"Invalid"' "$LOG"; then cat "$LOG"; die "$VAR: rejected by Apple"; fi
    [ "$i" = 90 ] && { tail -10 "$W/$VAR-notary.log"; die "$VAR: notarization still pending after 30 min"; }
    sleep 20
  done
  ok "$VAR notarized"
done

# 3. Verify exactly what users will run. Gatekeeper's online ticket lookup can serve a cached
#    "not notarized" answer for a few minutes right after notarization, so retry for up to 6 min.
echo "▸ Verifying signatures, tickets and Gatekeeper"
for VAR in $VARIANTS; do
  APP="$W/$VAR/LumenDisk.app"
  codesign --verify --deep --strict "$APP" || die "$VAR: signature invalid"
  SIG=$(codesign -dvv "$APP" 2>&1)   # captured first: `| grep -q` under pipefail can fail on SIGPIPE
  [[ "$SIG" == *"Authority=Developer ID Application: "*"($TEAM_ID)"* ]] || die "$VAR: not Developer ID signed"
  [[ "$SIG" =~ flags=[^[:space:]]*runtime ]] || die "$VAR: hardened runtime missing"
  xcrun stapler validate "$APP" >/dev/null || die "$VAR: no stapled ticket"
  # The stapled ticket must cover every slice of both executables.
  TICKET=$(xxd -p "$APP/Contents/CodeResources" | tr -d '\n')
  for b in Contents/MacOS/LumenDisk Contents/Helpers/lumendisk; do
    for a in $(lipo -archs "$APP/$b"); do
      H=$(codesign -dvvv --arch "$a" "$APP/$b" 2>&1 | sed -n 's/^CDHash=//p')
      [[ -n "$H" && "$TICKET" == *"$H"* ]] || die "$VAR: stapled ticket does not cover $b ($a, cdhash ${H:-?})"
    done
  done
  # Right after notarization Gatekeeper may still answer from a cached "not notarized" lookup
  # (seen lasting 5-10 minutes); retry for up to GATEKEEPER_WAIT_MIN (20) minutes before failing.
  GK_TRIES=$(( ${GATEKEEPER_WAIT_MIN:-20} * 6 ))
  for i in $(seq 1 $GK_TRIES); do
    spctl -a -t exec "$APP" 2>/dev/null && break
    [ "$i" = "$GK_TRIES" ] && { spctl -a -vv -t exec "$APP" || true; die "$VAR: rejected by Gatekeeper"; }
    [ "$i" = 1 ] && echo "    · $VAR: waiting for Gatekeeper to see the notarization ticket…"
    sleep 10
  done
  [ "$(lipo -archs "$APP/Contents/MacOS/LumenDisk")" = "$( [ $VAR = universal ] && echo 'x86_64 arm64' || echo $VAR)" ] || die "$VAR: wrong architectures"
  ok "$VAR: Developer ID, hardened runtime, ticket stapled, Gatekeeper: $(spctl -a -vv -t exec "$APP" 2>&1 | sed -n 's/^source=//p')"
done

# 4. The release must verify loopback clients for real: mount a test volume with the notarized
#    CLI and without the development opt-in. This only works if the kernel's NFS client is
#    positively identified (see SECURITY.md).
echo "▸ Mounting a test volume with the notarized build (verified loopback, no development opt-in)"
CLI="$W/universal/LumenDisk.app/Contents/Helpers/lumendisk"
T="$W/mounttest"; mkdir -p "$T/mnt"
( cd "$T" && env -u LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK "$CLI" mkfs ext4 t.img 32 RELCHECK >/dev/null )
( cd "$T" && env -u LUMENDISK_ALLOW_UNVERIFIED_LOOPBACK HOME="$T" "$CLI" mount t.img --root "$T/mnt" --control c.sock > mount.log 2>&1 & )
for _ in $(seq 1 100); do mount | grep -q "$T/mnt/RELCHECK" && break; sleep 0.2; done
mount | grep -q "$T/mnt/RELCHECK" || { cat "$T/mount.log"; die "the notarized build could not mount (loopback verification failed)"; }
echo verified > "$T/mnt/RELCHECK/check.txt"
[ "$(cat "$T/mnt/RELCHECK/check.txt")" = verified ] || die "write through the verified mount failed"
"$CLI" ctl "$T/c.sock" unmount >/dev/null
for _ in $(seq 1 50); do mount | grep -q "$T/mnt/RELCHECK" || break; sleep 0.2; done
! grep -q 'refused loopback' "$T/mount.log" || { cat "$T/mount.log"; die "a legitimate loopback client was refused"; }
ok "mounted, written and unmounted: the kernel NFS client was identified, nothing refused"

# 5. Package.
echo "▸ Packaging dist/release"
rm -rf "$OUT"; mkdir -p "$OUT"
for VAR in $VARIANTS; do
  mkdir -p "$W/dmg-$VAR"; ditto "$W/$VAR/LumenDisk.app" "$W/dmg-$VAR/LumenDisk.app"; ln -s /Applications "$W/dmg-$VAR/Applications"
  hdiutil create -quiet -volname "LumenDisk $V" -fs HFS+ -format UDZO -srcfolder "$W/dmg-$VAR" "$OUT/LumenDisk-$V-$VAR.dmg"
done
ditto -c -k --sequesterRsrc --keepParent "$W/universal/LumenDisk.app" "$OUT/LumenDisk-$V-universal.zip"
# The CLI is the Developer ID-signed helper from the notarized app (its cdhashes are in the ticket).
mkdir -p "$W/cli"; ditto "$W/universal/LumenDisk.app/Contents/Helpers/lumendisk" "$W/cli/lumendisk"
tar -czf "$OUT/lumendisk-cli-$V-macos-universal.tar.gz" -C "$W/cli" lumendisk
( cd "$OUT" && shasum -a 256 LumenDisk-$V-*.dmg LumenDisk-$V-*.zip lumendisk-cli-*.tar.gz > SHA256SUMS.txt )

# 6. Simulate a download: quarantine each DMG, mount it, and check the app inside with Gatekeeper.
for VAR in $VARIANTS; do
  D="$W/q-$VAR.dmg"; cp "$OUT/LumenDisk-$V-$VAR.dmg" "$D"
  xattr -w com.apple.quarantine "0081;$(printf %x "$(date +%s)");Safari;" "$D"
  ATT=$(hdiutil attach -nobrowse -readonly -noautoopen "$D")
  MP=$(printf '%s\n' "$ATT" | awk -F'\t' '/\/Volumes\// {print $NF}' | head -1)
  [ -n "$MP" ] || die "$VAR: DMG did not mount"
  R=$(spctl -a -vv -t exec "$MP/LumenDisk.app" 2>&1 | sed -n 's/^source=//p')
  hdiutil detach -quiet "$MP" || hdiutil detach -quiet -force "$MP"
  [ "$R" = "Notarized Developer ID" ] || die "$VAR DMG: Gatekeeper says '$R'"
  ok "LumenDisk-$V-$VAR.dmg downloaded + quarantined → Gatekeeper: $R"
done
ls -l "$OUT" | awk 'NR>1 {printf "  %8.1f MB  %s\n", $5/1048576, $9}'

if [ -n "${INSTALL:-}" ]; then
  DEST=/Applications/LumenDisk.app
  if [ -d "$DEST" ]; then
    BID=$($PB -c 'Print CFBundleIdentifier' "$DEST/Contents/Info.plist" 2>/dev/null || true)
    [ "$BID" = "$BUNDLE_ID" ] || die "$DEST exists and is not LumenDisk ($BID): not touching it"
    osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
    for _ in $(seq 1 20); do pgrep -f "$DEST/Contents/MacOS/LumenDisk" >/dev/null || break; sleep 0.5; done
    pgrep -f "$DEST/Contents/MacOS/LumenDisk" >/dev/null && die "LumenDisk is still running (volumes in use?): not replacing it"
    rm -rf "$DEST"
  fi
  ditto "$W/universal/LumenDisk.app" "$DEST" && open "$DEST"
  ok "installed and launched $DEST ($(spctl -a -vv -t exec "$DEST" 2>&1 | sed -n 's/^source=//p'))"
fi
