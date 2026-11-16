# FreeBSD test VM runner, used ONLY by tests (scripts/freebsd_check.sh): boots an official
# FreeBSD aarch64 VM image under qemu-system-aarch64 (TCG, Docker Desktop has no /dev/kvm) to
# check filesystems LumenDisk creates for FreeBSD (UFS2) with FreeBSD's own fsck_ffs / mount.
# The FreeBSD image itself is NOT in this Docker image: it is downloaded, checksum-verified and
# cached on the host under ~/.cache/lumendisk-test/freebsd/ and bind-mounted at run time.
FROM debian:trixie-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      qemu-system-arm qemu-efi-aarch64 qemu-utils xz-utils genisoimage coreutils procps \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /w
