; Test stand-in for Windows' BOOTMGR (core/tests/windows_e2e.rs).
;
; The NT6 partition boot records (ms-sys br_fat32pe / br_ntfs) find \BOOTMGR on the volume,
; load it at 2000:0000 and jump to it in real mode. This flat binary prints a marker on COM1
; (0x3F8, what QEMU's -serial captures) and through the BIOS teletype, then halts.
; It is position independent (the load segment is taken from CS) and padded to 16 KiB so
; that NTFS stores it non-resident, like the real BOOTMGR.
;
;   nasm -f bin -o bootmgr bootmgr.asm

bits 16
org 0

start:
    cli
    mov ax, cs
    mov ds, ax
    mov es, ax
    xor ax, ax
    mov ss, ax
    mov sp, 0x7000
    sti
    call .here
.here:
    pop bp
    sub bp, .here               ; bp = load offset inside CS
    lea si, [bp + msg]
.next:
    lodsb
    test al, al
    jz .halt
    mov cl, al
    mov dx, 0x3FD               ; COM1 line status: wait for an empty transmit register
.wait:
    in al, dx
    test al, 0x20
    jz .wait
    mov dx, 0x3F8
    mov al, cl
    out dx, al
    mov ah, 0x0E                ; BIOS teletype, for a screen
    mov bx, 0x0007
    int 0x10
    jmp .next
.halt:
    cli
    hlt
    jmp .halt

msg: db 13, 10, "LUMENDISK-BIOS-BOOTMGR-REACHED", 13, 10, 0

times 16384 - ($ - $$) db 0
