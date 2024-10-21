// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Entry point that zeroes BSS, sets up the stack, enables SSE, performs
// relocations, and jumps to start().
//
// BSS must be zeroed because the IGVM file does not contain pages for it, and
// during reboot there may be dirty data in memory. This must happen early
// because the stack itself is in BSS, so BSS must be zeroed before the stack is
// set up.

.globl _start
_start:
    mov     rbx, rdi                                // Save arg rdi
    lea     rdi, __bss_start[rip]                   // Put BSS base in rdi
    lea     rcx, _end[rip]                          // Put BSS end in rcx
    sub     rcx, rdi                                // Compute BSS len in rcx
    xor     eax, eax                                // Clear eax
    cld                                             // Clear the direction flag for the string operation
    rep     stosb                                   // Zero BSS: memset(rdi, al, rcx)
    mov     rdi, rbx                                // Restore rdi
    lea     rsp, {STACK_SIZE} + {stack}[rip]        // Set stack pointer
    mov     dword ptr {stack}[rip], {STACK_COOKIE}  // Set stack cookie
    mov     rax, cr4                                // Read CR4 into rax
    or      rax, 0x600                              // Set OSFXSR and OSXMMEXCPT for SSE support
    mov     cr4, rax                                // Set CR4 from rax with previous values set
    push    rsi                                     // caller save rsi
    push    rdi                                     // caller save rdi
    lea     rdx, _DYNAMIC[rip]                      // The start of the dynamic section, rip-relative
    lea     rdi, __ehdr_start[rip]                  // The mapped base of the image, rip-relative
    mov     rsi, rdi                                // The virtual address of the image
    call    {relocate}                              // call relocate to fixup relocation entries
    pop     rdi                                     // restore rdi (arg 0) to call start
    mov     rsi, [rsp]                              // restore rsi (arg 1) to call start (leave on stack to align)
    jmp     {start}                                 // jump to start
