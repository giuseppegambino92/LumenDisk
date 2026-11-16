# x86-only helpers for the Windows media tests (core/tests/windows_e2e.rs): they build the
# test boot payloads that the written media must reach in QEMU.
#   - grub-mkstandalone -O x86_64-efi: a tiny EFI application that prints a marker on the
#     console (stands in for Windows' \efi\boot\bootx64.efi);
#   - nasm: a 16-bit flat binary that prints a marker on COM1 (stands in for BOOTMGR, which
#     the NT6 partition boot records load at 2000:0000).
# grub-efi-amd64-bin only exists for x86, hence a separate amd64 image (Rosetta / QEMU user
# emulation on Apple silicon):
#   docker build --platform linux/amd64 -t lumendisk-wintest-amd64 scripts/docker/wintest-amd64
FROM --platform=linux/amd64 debian:trixie-slim
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      grub-common grub-efi-amd64-bin nasm mtools dosfstools \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /w
