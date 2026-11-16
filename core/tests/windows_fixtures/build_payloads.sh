#!/bin/sh
# Build the boot payloads of the Windows media tests, inside lumendisk-wintest-amd64
# (scripts/docker/wintest-amd64): run with this directory's files in /src and the output
# directory in /out.
#   /out/bootmgr      16-bit flat binary (marker on COM1), from bootmgr.asm
#   /out/bootx64.efi  GRUB x86_64-efi standalone image that echoes a marker, from grub.cfg
set -eu
nasm -f bin -o /out/bootmgr /src/bootmgr.asm
cp /src/grub.cfg /tmp/grub.cfg
grub-mkstandalone -O x86_64-efi \
    --locales= --fonts= --themes= \
    --install-modules="normal echo sleep halt configfile test" \
    --modules="normal echo sleep halt" \
    -o /out/bootx64.efi "boot/grub/grub.cfg=/tmp/grub.cfg"
ls -l /out
