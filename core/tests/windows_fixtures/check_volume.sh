#!/bin/sh
# Check a Windows media partition written by LumenDisk with the Linux tools (privileged
# lumendisk-toolbox: loop devices and the kernel's vfat / ntfs3 drivers):
#   check_volume.sh DISK OFFSET SIZE fat32|ntfs MEDIA_DIR [ORIG_WIM]
# - fsck.vfat -n / ntfsfix -n on the partition;
# - every file of MEDIA_DIR is on the volume with the same SHA-256 (install.wim excepted when
#   it was split);
# - split sets: wimlib-imagex info + verify, and every image applied from the .swm set is
#   identical to the same image applied from ORIG_WIM.
# Prints FSCK-OK, HASHES-OK, SWM-VERIFY-OK, APPLY-OK n, ALL-OK.
set -eu
disk=$1
off=$2
size=$3
fs=$4
media=$5
orig=${6:-}
loop=$(losetup -f --show -o "$off" --sizelimit "$size" "$disk")
mnt=$(mktemp -d)
work=$(mktemp -d)
cleanup() {
    umount "$mnt" 2>/dev/null || true
    losetup -d "$loop" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT
if [ "$fs" = fat32 ]; then
    fsck.vfat -n "$loop"
    echo FSCK-OK
    mount -t vfat -o ro,utf8 "$loop" "$mnt"
else
    ntfsfix -n "$loop"
    echo FSCK-OK
    mount -t ntfs3 -o ro,iocharset=utf8 "$loop" "$mnt"
fi
split=0
[ -e "$mnt/sources/install.swm" ] && split=1
(cd "$media" && find . -type f -exec sha256sum {} + | sort -k2) > "$work/src.sum"
(cd "$mnt" && find . -type f ! -name 'install*.swm' ! -name autounattend.xml -exec sha256sum {} + | sort -k2) > "$work/dst.sum"
if [ $split = 1 ]; then
    grep -v ' ./sources/install.wim$' "$work/src.sum" > "$work/src2.sum"
    mv "$work/src2.sum" "$work/src.sum"
fi
diff "$work/src.sum" "$work/dst.sum"
echo "HASHES-OK $(wc -l < "$work/dst.sum") files"
if [ $split = 1 ]; then
    ls -l "$mnt/sources/"install*.swm
    wimlib-imagex info "$mnt/sources/install.swm" | grep -E 'GUID|Part Number|Image Count|Boot Index|Compression'
    for p in "$mnt/sources/"install*.swm; do
        wimlib-imagex info "$p" --header | grep -E '^(GUID|Part Number|Total Parts|Image Count|Boot Index)'
    done
    wimlib-imagex verify "$mnt/sources/install.swm" --ref="$mnt/sources/install*.swm"
    echo SWM-VERIFY-OK
    if [ -n "$orig" ]; then
        n=$(wimlib-imagex info "$orig" | sed -n 's/^Image Count: *//p')
        i=1
        while [ "$i" -le "$n" ]; do
            wimlib-imagex apply "$orig" "$i" "$work/a$i" >/dev/null
            wimlib-imagex apply "$mnt/sources/install.swm" "$i" "$work/b$i" --ref="$mnt/sources/install*.swm" >/dev/null
            diff -r "$work/a$i" "$work/b$i"
            (cd "$work/a$i" && find . -type f -exec sha256sum {} + | sort -k2) > "$work/a$i.sum"
            (cd "$work/b$i" && find . -type f -exec sha256sum {} + | sort -k2) > "$work/b$i.sum"
            cmp "$work/a$i.sum" "$work/b$i.sum"
            rm -rf "$work/a$i" "$work/b$i"
            i=$((i + 1))
        done
        echo "APPLY-OK $n images"
    fi
fi
if [ -e "$mnt/autounattend.xml" ]; then
    cp "$mnt/autounattend.xml" "$work/autounattend.xml"
    echo "AUTOUNATTEND-PRESENT"
    cp "$work/autounattend.xml" /w/autounattend.out.xml
fi
echo ALL-OK
