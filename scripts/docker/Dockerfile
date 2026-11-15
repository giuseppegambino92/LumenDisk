# Reference toolbox used ONLY by the verification script to create "real" filesystems
# with upstream Linux tools and to independently fsck what LumenDisk wrote.
FROM debian:trixie-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      e2fsprogs ntfs-3g dosfstools exfatprogs mtools fdisk util-linux python3 openssh-server apache2 apache2-utils procps \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /w
