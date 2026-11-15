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
trap 'rm -rf "$W"' EXIT
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
  xcodebuild -exportArchive -archivePath "$A" -exportOptionsPlist "$W/export.plist" -exportPath "$W/$VAR-upload" \
    -allowProvisioningUpdates > "$W/$VAR-upload.log" 2>&1 || { tail -20 "$W/$VAR-upload.log"; die "$VAR: upload failed"; }
  grep -q 'EXPORT SUCCEEDED' "$W/$VAR-upload.log" || { tail -20 "$W/$VAR-upload.log"; die "$VAR: upload failed"; }
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
  for i in $(seq 1 36); do
    spctl -a -t exec "$APP" 2>/dev/null && break
    [ "$i" = 36 ] && { spctl -a -vv -t exec "$APP"; die "$VAR: rejected by Gatekeeper"; }
    sleep 10
  done
  [ "$(lipo -archs "$APP/Contents/MacOS/LumenDisk")" = "$( [ $VAR = universal ] && echo 'x86_64 arm64' || echo $VAR)" ] || die "$VAR: wrong architectures"
  ok "$VAR: Developer ID, hardened runtime, ticket stapled, Gatekeeper: $(spctl -a -vv -t exec "$APP" 2>&1 | sed -n 's/^source=//p')"
done

# 4. Package.
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

# 5. Simulate a download: quarantine each DMG, mount it, and check the app inside with Gatekeeper.
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
