# Extra checks for core/tests/rpi_wipe_e2e.rs (image `lumendisk-rpi-qa`, built by the test when
# missing): the parts of the Raspberry Pi customisation that `lumendisk-toolbox` can't check.
# - netplan's own parser (python3-netplan): `cloud-init schema -t network-config` validates a
#   v2 config (wifis, regulatory-domain, hidden) through it; without it cloud-init's JSON
#   schema knows no `wifis` at all.
# - shellcheck for firstrun.sh.
# - qemu-system-aarch64 to boot a customised image far enough for cloud-init / firstrun.
FROM debian:trixie-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      cloud-init netplan.io python3-netplan shellcheck whois openssl python3 \
      dosfstools e2fsprogs mtools fdisk xz-utils zstd cpio qemu-utils qemu-system-arm ipxe-qemu \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /w
