// Copyright (C) Microsoft Corporation. All rights reserved.
//
// Entry point that zeroes BSS, sets up the stack, performs relocations,
// does architecture-specific setup, and jumps to start().
//
// BSS must be zeroed because the IGVM file does not contain pages for it, and
// during reboot there may be dirty data in memory. This must happen early
// because the stack itself is in BSS, so BSS must be zeroed before the stack is
// set up.

    .weak _DYNAMIC
    .hidden _DYNAMIC

    .balign 0x10
.globl _start
_start:

    // Clean BSS, avoid using x0 as it contains the IGVM parameter.
    // NOTE: the stack space is allocated in BSS, and can't use function calls
    // as the return address will be wiped out.

    adrp    x1,  __bss_start__
    add     x1, x1, :lo12:__bss_start__ // X1 contains the BSS start
    adrp    x2, __bss_end__
    add     x2, x2, :lo12:__bss_end__
    sub     x2, x2, x1                  // X2 contains the BSS length
1:
    cbz     x2, 2f
    sub     x2, x2, 1
    strb    wzr, [x1,x2]
    b       1b
2:

    // Set up the stack space.

    adrp    x1, {stack}
    add     x1, x1, :lo12:{stack}
    mov     x2, {STACK_COOKIE_LO}           // Lower 16 bits of the stack cookie
    movk    x2, {STACK_COOKIE_HI}, lsl 16   // Higher 16 bits of the stack cookie, keep the lower bits
    str     x2, [x1]                        // Store the stack cookie at the bottom
    add     x1, x1, {STACK_SIZE}            // Stack size
    sub     x1, x1, #8                      // Leave 8 bytes for the stack cookie at the top
    str     x2, [x1]                        // Store the stack cookie at the top
    sub     x1, x1, #8                      // Set the stack pointer
    mov     sp, x1

    // Set the vector table up.

    adrp    x1, _vector_table_el1
    add     x1, x1, :lo12:_vector_table_el1
    msr     VBAR_EL1, x1
    isb

    // Push x0 to the stack, its value has to be passed to `start`.

    str     x0, [sp, #-16]!

    // NEON and FP setup for EL1. The compiler can use SIMD as an
    // optimization because the target specific options set in the `rustc`
    // do not prohibit that.
    // This is not compiled for the `softfloat` target so enabling FP
    // for consistency.

    mrs     x0, CPACR_EL1
    orr     x0, x0, #(3 << 20)
    orr     x0, x0, #(3 << 16)
    msr     CPACR_EL1, x0
    isb

    // Call `relocate` to fixup relocation entries. The Rust compiler
    // produces globals for the formatting calls.

    adrp    x0, __ehdr_start
    add     x0, x0, :lo12:__ehdr_start
    mov     x1, x0
    adrp    x2, _DYNAMIC
    add     x2, x2, :lo12:_DYNAMIC
    bl      {relocate}

    // Restore the IGVM parameter from the stack and call the main function.
    // Its first parameter is ignored.

    mov     x0, xzr
    ldr     x1, [sp], #16
    bl      {start}

    // If the main function exited, call into the Debug Interface, or
    // break.

    mov     x0, 6
    movk    x0, 0x8600, lsl 16
    smc     #0

    .macro EXCEPTION_ENTRY source, kind
    .align 7
        b       .
        mov     x0, \source
        mov     x1, \kind
        b       .
    .endm

    // Vector table must be aligned to a 2KB boundary.
    .balign 0x800
_vector_table_el1:
    // Target and source at same exception level with source SP = SP_EL0
    EXCEPTION_ENTRY #0x0, #0x0  // Synchronous exception
    EXCEPTION_ENTRY #0x0, #0x1  // IRQ
    EXCEPTION_ENTRY #0x0, #0x2  // FIQ
    EXCEPTION_ENTRY #0x0, #0x3  // SError

    // Target and source at same exception level with source SP = SP_ELx
    EXCEPTION_ENTRY #0x1, #0x0  // Synchronous exception
    EXCEPTION_ENTRY #0x1, #0x1  // IRQ
    EXCEPTION_ENTRY #0x1, #0x2  // FIQ
    EXCEPTION_ENTRY #0x1, #0x3  // SError

    // Source is at lower exception level running on AArch64
    EXCEPTION_ENTRY #0x2, #0x0  // Synchronous exception
    EXCEPTION_ENTRY #0x2, #0x1  // IRQ
    EXCEPTION_ENTRY #0x2, #0x2  // FIQ
    EXCEPTION_ENTRY #0x2, #0x3  // SError

    // Source is at lower exception level running on AArch32
    EXCEPTION_ENTRY #0x3, #0x0  // Synchronous exception
    EXCEPTION_ENTRY #0x3, #0x1  // IRQ
    EXCEPTION_ENTRY #0x3, #0x2  // FIQ
    EXCEPTION_ENTRY #0x3, #0x3  // SError
