# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Credits a 64-byte /tmp/entropy payload, then immediately reseeds the CRNG.
# Regenerate the adjacent fixture with:
#   as --64 -o openvmm-reseed.o openvmm-reseed.S
#   ld -N -s -o openvmm-reseed openvmm-reseed.o
#   base64 -w0 openvmm-reseed > openvmm-reseed.b64
.global _start
.section .text
_start:
    mov $2, %rax
    lea entropy_path(%rip), %rdi
    xor %rsi, %rsi
    xor %rdx, %rdx
    syscall
    test %rax, %rax
    js fail
    mov %rax, %rdi
    xor %rax, %rax
    lea entropy_buf+8(%rip), %rsi
    mov $64, %rdx
    syscall
    cmp $64, %rax
    jne fail
    mov $2, %rax
    lea random_path(%rip), %rdi
    mov $1, %rsi
    xor %rdx, %rdx
    syscall
    test %rax, %rax
    js fail
    mov %rax, %rdi
    mov $16, %rax
    mov $0x40085203, %rsi
    lea entropy_buf(%rip), %rdx
    syscall
    test %rax, %rax
    js fail
    mov $16, %rax
    mov $0x5207, %rsi
    xor %rdx, %rdx
    syscall
    test %rax, %rax
    js fail
    xor %rdi, %rdi
    jmp exit
fail:
    mov $1, %rdi
exit:
    mov $60, %rax
    syscall
.section .rodata
entropy_path: .asciz "/tmp/entropy"
random_path: .asciz "/dev/random"
.section .data
.align 8
entropy_buf:
    .long 512
    .long 64
    .zero 64
