#!/bin/sh
# Build a Windows-11-like install.wim with wimlib (inside lumendisk-toolbox):
#   make_wim.sh OUT.wim SIZE_MIB COMPRESSION [FILE_MIB]
# Three images (Home / Pro / Education) sharing half of their data (so blobs are
# deduplicated, as in real install.wims), with the XML metadata Windows Setup reads
# (edition id, build, architecture, languages…), of about SIZE_MIB MiB of incompressible data
# in FILE_MIB MiB files.
set -eu
out=$1
mb=$2
comp=$3
fmb=${4:-1}
t=$(mktemp -d)
trap 'rm -rf "$t"' EXIT
mkdir -p "$t/common/Windows/System32/drivers" "$t/common/Program Files/Common Files" "$t/own"
n=$((mb / fmb))
shared=$((n / 2))
own=$(((n - shared) / 3))
i=0
while [ $i -lt $shared ]; do
    head -c $((fmb * 1048576)) /dev/urandom > "$t/common/Windows/System32/f$i.dll"
    i=$((i + 1))
done
echo "shared text" > "$t/common/Windows/win.ini"
mkdir -p "$t/common/Users/Default/Desktop"
printf 'n\xc3\xa4me' > "$t/common/Users/Default/Desktop/\xc3\xbcn\xc3\xafc\xc3\xb6d\xc3\xa9.txt"
for ed in Core Professional Education; do
    mkdir -p "$t/img-$ed"
    cp -al "$t/common/." "$t/img-$ed/"
    mkdir -p "$t/img-$ed/Windows/Edition"
    i=0
    while [ $i -lt $own ]; do
        head -c $((fmb * 1048576)) /dev/urandom > "$t/img-$ed/Windows/Edition/$ed-$i.bin"
        i=$((i + 1))
    done
    echo "$ed" > "$t/img-$ed/Windows/Edition/EditionID.txt"
done
add() {
    # $1 capture|append, $2 edition id, $3 image name; extra args follow
    cmd=$1
    ed=$2
    name=$3
    shift 3
    wimlib-imagex "$cmd" "$t/img-$ed" "$out" "$name" "$@" \
        --image-property "DESCRIPTION=$name" --image-property "DISPLAYNAME=$name" \
        --image-property "DISPLAYDESCRIPTION=$name" --image-property "FLAGS=$ed" \
        --image-property "WINDOWS/ARCH=9" --image-property "WINDOWS/EDITIONID=$ed" \
        --image-property "WINDOWS/INSTALLATIONTYPE=Client" --image-property "WINDOWS/PRODUCTTYPE=WinNT" \
        --image-property "WINDOWS/PRODUCTNAME=Microsoft® Windows® Operating System" \
        --image-property "WINDOWS/LANGUAGES/LANGUAGE=en-US" --image-property "WINDOWS/LANGUAGES/DEFAULT=en-US" \
        --image-property "WINDOWS/VERSION/MAJOR=10" --image-property "WINDOWS/VERSION/MINOR=0" \
        --image-property "WINDOWS/VERSION/BUILD=26100" --image-property "WINDOWS/VERSION/SPBUILD=1742" \
        --image-property "WINDOWS/VERSION/SPLEVEL=0" --image-property "WINDOWS/SYSTEMROOT=WINDOWS" >/dev/null
}
rm -f "$out"
add capture Core "Windows 11 Home" --compress="$comp"
add append Professional "Windows 11 Pro"
add append Education "Windows 11 Education"
ls -l "$out"
