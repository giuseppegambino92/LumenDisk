# Reference toolbox used ONLY by the verification script and the tests: it creates "real"
# filesystems, ISOs and images with upstream tools, independently checks what LumenDisk wrote
# (fsck of every filesystem, wimlib, cloud-init schema), and boots written media in QEMU.
FROM debian:trixie-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      e2fsprogs ntfs-3g dosfstools exfatprogs mtools fdisk util-linux python3 openssh-server apache2 apache2-utils procps \
      btrfs-progs xfsprogs f2fs-tools udftools gdisk parted kpartx \
      wimtools xorriso genisoimage squashfs-tools cpio xz-utils zstd bzip2 zip unzip p7zip-full qemu-utils \
      syslinux-common syslinux-efi isolinux grub-common \
      qemu-system-x86 ovmf \
      cloud-init whois openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /w
