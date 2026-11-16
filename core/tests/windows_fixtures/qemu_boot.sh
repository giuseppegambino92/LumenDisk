#!/bin/sh
# Boot a disk image in QEMU (x86_64, TCG) from a USB stick and wait for a marker on the
# serial console (inside lumendisk-toolbox):
#   qemu_boot.sh bios|uefi|uefi-sb DISK LOG MARKER TIMEOUT_SECS [LINGER_SECS]
# bios: SeaBIOS; uefi: OVMF; uefi-sb: OVMF with Secure Boot enforced and Microsoft's keys
# enrolled (OVMF_CODE_4M.ms.fd + OVMF_VARS_4M.ms.fd, SMM). The disk is opened with
# snapshot=on: nothing is written to it. Prints MARKER-FOUND / MARKER-NOT-FOUND and the
# console output (escape sequences stripped).
set -u
mode=$1
disk=$2
log=$3
marker=$4
secs=$5
linger=${6:-1}
rm -f "$log"
vars=$(mktemp)
case $mode in
bios)
    set -- -machine pc
    ;;
uefi)
    cp /usr/share/OVMF/OVMF_VARS_4M.fd "$vars"
    set -- -machine q35 \
        -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd \
        -drive if=pflash,format=raw,unit=1,file="$vars"
    ;;
uefi-sb)
    cp /usr/share/OVMF/OVMF_VARS_4M.ms.fd "$vars"
    set -- -machine q35,smm=on -global driver=cfi.pflash01,property=secure,value=on \
        -drive if=pflash,format=raw,unit=0,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.ms.fd \
        -drive if=pflash,format=raw,unit=1,file="$vars"
    ;;
*)
    echo "unknown mode $mode"
    exit 2
    ;;
esac
qemu-system-x86_64 "$@" -m 512 -display none -monitor none -serial "file:$log" -net none \
    -drive if=none,id=stick,format=raw,snapshot=on,file="$disk" \
    -device qemu-xhci -device usb-storage,drive=stick,bootindex=0 &
pid=$!
i=0
found=0
while [ $i -lt "$secs" ]; do
    if grep -q "$marker" "$log" 2>/dev/null; then
        found=1
        sleep "$linger"
        break
    fi
    kill -0 $pid 2>/dev/null || break
    sleep 1
    i=$((i + 1))
done
kill $pid 2>/dev/null
wait $pid 2>/dev/null
rm -f "$vars"
if [ $found = 1 ]; then echo "MARKER-FOUND after ${i}s"; else echo "MARKER-NOT-FOUND after ${i}s"; fi
echo "----- console ($mode) -----"
sed 's/\x1b\[[0-9;?]*[A-Za-z]//g' "$log" | tr -d '\r' | grep -v '^[[:space:]]*$' | tail -n 40
[ $found = 1 ]
