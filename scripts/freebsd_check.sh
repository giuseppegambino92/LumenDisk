#!/bin/sh
# Run a shell script as root inside a real FreeBSD, with disk images attached.
#
#   scripts/freebsd_check.sh <guest-script.sh> [<disk.img> ...]
#
# Used by the tests to check filesystems LumenDisk creates for FreeBSD (UFS2) with FreeBSD's own
# tools (fsck_ffs, mount, newfs as a reference). It boots the official FreeBSD aarch64
# BASIC-CLOUDINIT VM image under qemu-system-aarch64 (TCG: Docker Desktop has no /dev/kvm)
# inside the `lumendisk-freebsd` Docker image (scripts/docker/freebsd/Dockerfile), and hands
# the script to the guest through a NoCloud `cidata` ISO: FreeBSD's nuageinit writes it (cloud-config
# `write_files`) and runs it as root at first boot (`runcmd`, at the end of /etc/rc).
#
# Guest environment (the script runs with /bin/sh, cwd /root):
#   NDISKS          number of attached images
#   DISK1..DISKn    their device nodes (e.g. /dev/vtbd2), in the order given on the command line.
#                   Mapped through the virtio-blk serial number (`diskinfo -s`), not guessed.
#
# Output: the script's stdout+stderr is streamed to this script's stdout (tagged lines on the
# serial console, console noise filtered out). The exit status is the script's exit status;
# 125 if the VM did not boot / the script did not finish within FREEBSD_TIMEOUT seconds
# (default 1200), with the console tail on stderr. Writes by the guest persist in the images.
# Boot and total wall time are printed on stderr.
#
# Environment: FREEBSD_RELEASE (default 14.5), FREEBSD_TIMEOUT (seconds), FREEBSD_SMP (default
# 4), FREEBSD_MEM (MiB, default 2048), FREEBSD_CONSOLE=1 (echo the whole serial console,
# timestamped, on stderr: for diagnosing boot problems), LUMENDISK_TEST_CACHE (default
# ~/.cache/lumendisk-test; must be a directory Docker Desktop can bind-mount).
#
# The FreeBSD image is downloaded once from download.freebsd.org, verified against the
# published CHECKSUM.SHA512 and cached (decompressed, read-only) under
# ~/.cache/lumendisk-test/freebsd/ (~0.6 GB download, ~2.2 GB decompressed). Each run boots a throwaway qcow2 overlay on top of it.
set -eu

usage() { echo "usage: $0 <guest-script.sh> [<disk.img> ...]" >&2; exit 2; }
[ $# -ge 1 ] || usage
GUEST=$1; shift
[ -f "$GUEST" ] || { echo "freebsd_check: no such script: $GUEST" >&2; exit 2; }

REL=${FREEBSD_RELEASE:-14.5}
TIMEOUT=${FREEBSD_TIMEOUT:-1200}
SMP=${FREEBSD_SMP:-4}
MEM=${FREEBSD_MEM:-2048}
CONSOLE=${FREEBSD_CONSOLE:-0}
IMAGE_TAG=lumendisk-freebsd
HERE=$(cd "$(dirname "$0")" && pwd -P)
CACHE=${LUMENDISK_TEST_CACHE:-$HOME/.cache/lumendisk-test}/freebsd
NAME=FreeBSD-$REL-RELEASE-arm64-aarch64-BASIC-CLOUDINIT-ufs.qcow2
URL=https://download.freebsd.org/releases/VM-IMAGES/$REL-RELEASE/aarch64/Latest
SUMS=$CACHE/CHECKSUM.SHA512.$REL-aarch64
T0=$(date +%s)

log() { echo "freebsd_check: $*" >&2; }

# ───────────── Docker image ─────────────
if ! docker image inspect "$IMAGE_TAG" >/dev/null 2>&1; then
  log "building Docker image $IMAGE_TAG"
  docker build -q -t "$IMAGE_TAG" "$HERE/docker/freebsd" >&2
fi

# ───────────── FreeBSD image: download, verify, decompress (cached) ─────────────
sha512_of() {
  if command -v sha512sum >/dev/null 2>&1; then sha512sum "$1" | cut -d' ' -f1
  else shasum -a 512 "$1" | cut -d' ' -f1; fi
}
mkdir -p "$CACHE"
if [ ! -f "$CACHE/$NAME.verified" ] || [ ! -f "$CACHE/$NAME" ]; then
  if [ ! -s "$SUMS" ]; then
    log "fetching $URL/CHECKSUM.SHA512"
    curl -fsSL -o "$SUMS.part" "$URL/CHECKSUM.SHA512"
    mv "$SUMS.part" "$SUMS"
  fi
  WANT=$(sed -n "s/^SHA512 ($NAME.xz) = \([0-9a-f]*\)\$/\1/p" "$SUMS")
  [ -n "$WANT" ] || { log "no checksum for $NAME.xz in $SUMS"; exit 125; }
  if [ ! -f "$CACHE/$NAME.xz" ] || [ "$(sha512_of "$CACHE/$NAME.xz")" != "$WANT" ]; then
    log "downloading $URL/$NAME.xz"
    curl -fL --retry 3 -o "$CACHE/$NAME.xz.part" "$URL/$NAME.xz"
    GOT=$(sha512_of "$CACHE/$NAME.xz.part")
    if [ "$GOT" != "$WANT" ]; then
      rm -f "$CACHE/$NAME.xz.part"
      log "SHA512 mismatch for $NAME.xz (got $GOT, want $WANT)"; exit 125
    fi
    mv "$CACHE/$NAME.xz.part" "$CACHE/$NAME.xz"
  fi
  log "SHA512 of $NAME.xz verified; decompressing"
  rm -f "$CACHE/$NAME" "$CACHE/$NAME.tmp"
  docker run --rm -v "$CACHE:/c" -w /c "$IMAGE_TAG" sh -c "xz -dc '$NAME.xz' > '$NAME.tmp'"
  mv "$CACHE/$NAME.tmp" "$CACHE/$NAME"
  chmod a-w "$CACHE/$NAME"      # the base is only ever used as a read-only backing file
  echo "$WANT" > "$CACHE/$NAME.verified"
fi

# ───────────── per-run directory (cleaned up on exit) ─────────────
mkdir -p "$CACHE/runs"
RUN=$(mktemp -d "$CACHE/runs/run.XXXXXX")
CNAME=lumendisk-freebsd-run-$(basename "$RUN" | tr -c 'A-Za-z0-9\n' '-')
cleanup() {
  docker rm -f "$CNAME" >/dev/null 2>&1 || true
  rm -rf "$RUN"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

TAG=LUMENOUT$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')

# Disks: bind-mounted one by one, attached as virtio-blk with serial lumendisk<N>. The
# positional parameters are rotated into the docker `-v` arguments.
N=0
for f do
  shift
  [ -f "$f" ] || { log "no such image: $f"; exit 2; }
  N=$((N + 1))
  abs=$(cd "$(dirname "$f")" && pwd -P)/$(basename "$f")
  set -- "$@" -v "$abs:/disks/$N.img"
done

# ───────────── cidata: cloud-config for nuageinit ─────────────
# nuageinit (rc.d/nuageinit, before NETWORKING) writes the files; `runcmd` runs at the end of
# /etc/rc. The image enables firstboot_freebsd_update, which with no network spends ~20 s on DNS
# timeouts before runcmd: it is switched off through /etc/rc.conf.d (read when it runs).
mkdir -p "$RUN/cidata"
printf 'instance-id: lumendisk-%s\nlocal-hostname: lumenbsd\n' "$TAG" > "$RUN/cidata/meta-data"
{
  cat <<EOF
#!/bin/sh
# Generated by scripts/freebsd_check.sh: maps the disks, runs /root/guest.sh, tags its output
# on the console, reports its exit status and powers off.
TAG=$TAG
N=$N
EOF
  cat <<'EOF'
echo "$TAG B"
# Map lumendisk<i> (virtio-blk serial) → device node.
NDISKS=$N; export NDISKS
i=1
while [ "$i" -le "$N" ]; do
  dev=""
  for d in /dev/vtbd[0-9] /dev/vtbd[0-9][0-9]; do
    [ -c "$d" ] || continue
    if [ "$(diskinfo -s "$d" 2>/dev/null)" = "lumendisk$i" ]; then dev=$d; fi
  done
  if [ -z "$dev" ]; then echo "$TAG O freebsd_check: disk lumendisk$i not found"; echo "$TAG X 125"; sync; shutdown -p now; exit 0; fi
  eval "DISK$i=$dev; export DISK$i"
  i=$((i + 1))
done
cd /root
rc_file=/tmp/lumen_rc
( /bin/sh /root/guest.sh; echo $? > "$rc_file" ) 2>&1 | while IFS= read -r line || [ -n "$line" ]; do
  printf '%s O %s\n' "$TAG" "$line"
done
rc=$(cat "$rc_file" 2>/dev/null || echo 125)
sync
echo "$TAG X $rc"
shutdown -p now
EOF
} > "$RUN/wrapper.sh"
b64() { base64 < "$1" | tr -d '\n'; }
cat > "$RUN/cidata/user-data" <<EOF
#cloud-config
write_files:
  - path: /etc/rc.conf.d/firstboot_freebsd_update
    content: "firstboot_freebsd_update_enable=NO\\n"
  - path: /root/lumen_wrapper.sh
    encoding: b64
    content: $(b64 "$RUN/wrapper.sh")
  - path: /root/guest.sh
    encoding: b64
    content: $(b64 "$GUEST")
runcmd:
  - /bin/sh /root/lumen_wrapper.sh
EOF

# ───────────── runner inside the container ─────────────
cat > "$RUN/runner.sh" <<'EOF'
#!/bin/sh
# $1 = tag, $2 = number of disks, $3 = timeout, $4 = smp, $5 = mem, $6 = base image name,
# $7 = 1 to echo the whole console (timestamped) on stderr
TAG=$1; N=$2; TIMEOUT=$3; SMP=$4; MEM=$5; BASE=/c/$6; CON=$7
cd /run/lumen
genisoimage -quiet -output seed.iso -volid cidata -joliet -rock cidata/user-data cidata/meta-data
qemu-img create -q -f qcow2 -F qcow2 -b "$BASE" root.qcow2
cp /usr/share/AAVMF/AAVMF_VARS.fd vars.fd
set -- -M virt -accel tcg,thread=multi -cpu max,pauth-impdef=on -smp "$SMP" -m "$MEM" \
  -drive if=pflash,format=raw,readonly=on,file=/usr/share/AAVMF/AAVMF_CODE.no-secboot.fd \
  -drive if=pflash,format=raw,file=vars.fd \
  -drive if=none,id=root,file=root.qcow2,format=qcow2 -device virtio-blk-pci,drive=root,serial=root \
  -drive if=none,id=seed,file=seed.iso,format=raw,readonly=on -device virtio-blk-pci,drive=seed,serial=cidata \
  -nic none -display none -monitor none -serial file:console.log
i=1
while [ "$i" -le "$N" ]; do
  set -- "$@" -drive if=none,id=d$i,file=/disks/$i.img,format=raw,cache=writeback \
    -device virtio-blk-pci,drive=d$i,serial=lumendisk$i
  i=$((i + 1))
done
: > console.log
start=$(date +%s)
timeout "$TIMEOUT" qemu-system-aarch64 "$@" &
QPID=$!
# Follow the console until qemu exits; print the guest's tagged lines live. (mawk -W interactive:
# line-buffered in and out, so the output is really live and the boot time is right.)
tail --pid=$QPID -n +1 -f console.log | mawk -W interactive -v tag="$TAG" -v start="$start" -v con="$CON" '
  { gsub(/\r/, "") }
  index($0, tag " B") == 1 { "date +%s" | getline now; close("date +%s"); print now - start > "boot_secs"; close("boot_secs"); next }
  index($0, tag " O ") == 1 { print substr($0, length(tag) + 4); fflush(); next }
  index($0, tag " O") == 1 && length($0) == length(tag) + 2 { print ""; fflush(); next }
  index($0, tag " X ") == 1 { print substr($0, length(tag) + 4) > "exit_code"; close("exit_code"); next }
  con == 1 { "date +%s" | getline now; close("date +%s"); print "[" now - start "s] " $0 > "/dev/stderr"; fflush("/dev/stderr") }
'
wait $QPID
qrc=$?
echo "$qrc" > qemu_rc
echo $(( $(date +%s) - start )) > vm_secs
EOF

log "booting FreeBSD $REL (aarch64, TCG) with $N disk(s)"
docker run --rm --name "$CNAME" -v "$CACHE:/c:ro" -v "$RUN:/run/lumen" "$@" \
  "$IMAGE_TAG" sh /run/lumen/runner.sh "$TAG" "$N" "$TIMEOUT" "$SMP" "$MEM" "$NAME" "$CONSOLE" || true

BOOT=$(cat "$RUN/boot_secs" 2>/dev/null || echo "?")
VM=$(cat "$RUN/vm_secs" 2>/dev/null || echo "?")
log "boot ${BOOT}s, VM ${VM}s, total $(( $(date +%s) - T0 ))s"
if [ -s "$RUN/exit_code" ]; then
  RC=$(cat "$RUN/exit_code")
  case "$RC" in ''|*[!0-9]*) RC=125 ;; esac
  exit "$RC"
fi
log "the guest script did not finish (qemu exit $(cat "$RUN/qemu_rc" 2>/dev/null || echo '?'), timeout ${TIMEOUT}s); console tail:"
tr -d '\r' < "$RUN/console.log" 2>/dev/null | tail -40 >&2 || true
exit 125
