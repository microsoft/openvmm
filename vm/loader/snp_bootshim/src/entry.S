// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// SNP bootshim entry point. The initial VMSA starts here in 64-bit mode with
// encrypted identity mappings already active and RSI pointing to the measured
// bootshim parameter page. This code preserves that pointer, zeroes BSS,
// installs a private stack, applies static-PIE relocations, and transfers
// control to the Rust acceptance routine.

.globl _start
_start:
    mov     r12, rsi                               // Save the bootshim parameter-page GPA
    lea     rdi, __bss_start[rip]                  // Put the BSS base in rdi
    lea     rcx, _end[rip]                         // Put the BSS end in rcx
    sub     rcx, rdi                               // Compute the BSS length in rcx
    xor     eax, eax                               // Clear eax for the string operation
    cld                                            // Clear the direction flag for stosb
    rep     stosb                                  // Zero BSS: memset(rdi, 0, rcx)
    lea     rsp, {STACK_SIZE} + {stack}[rip]       // Install the bootshim stack
    and     rsp, -16                               // Align the stack to a 16-byte boundary
    lea     rdx, _DYNAMIC[rip]                     // Put the dynamic section address in rdx
    lea     rdi, __ehdr_start[rip]                 // Put the loaded image base in rdi
    mov     rsi, rdi                               // Use the loaded image base as the virtual base
    call    {relocate}                             // Apply static-PIE relocation entries
    sub     rsp, 8                                 // Match the SysV function-entry stack alignment
    mov     rdi, r12                               // Pass the parameter-page GPA to start()
    jmp     {start}                                // Enter Rust
