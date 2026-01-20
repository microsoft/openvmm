// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//
// Entry point that zeroes BSS, sets up the stack, performs relocations,
// does architecture-specific setup, and jumps to start().
//
// BSS must be zeroed because the IGVM file does not contain pages for it, and
// during reboot there may be dirty data in memory. This must happen early
// because the stack itself is in BSS, so BSS must be zeroed before the stack is
// set up.

    // Not all distro's have toolchains whose linker script facilitates emitting
    // _DYNAMIC. In case of the linking failures due to missing _DYNAMIC, blame on
    // the toolchain's default linker script (maybe we should provide our own one).
    //
    // Adding something like ".weak _DYNAMIC" masks that issue, and makes the bootloader
    // fail when relocating the image as that won't point to the correct location.

    .extern _DYNAMIC

    // Calls into the platform's debug interface

    .macro SMC_CALL_DEBUG_INTERFACE
        mov     x0, 6
        movk    x0, 0x8600, lsl 16
        smc     #0
        brk     #0xffff
    .endm

    .balign 0x10
.globl _start
_start:

    // Save the x0 which contains the IGVM parameter to the first scratch non-volatile register.

    mov     x19, x0

    // Set the EL1 vector base address register to point to the vector table
    // at the earliest possible point.

    adrp    x0, _vector_table_el1
    add     x0, x0, :lo12:_vector_table_el1
    msr     VBAR_EL1, x0
    isb

    // Clean BSS.
    // NOTE: the stack space is allocated in BSS, and one can't use function calls yet
    // as the return address will be wiped out.

    adrp    x0,  __bss_start
    add     x0, x0, :lo12:__bss_start // X0 contains the BSS start
    adrp    x1, _end
    add     x1, x1, :lo12:_end
    sub     x1, x1, x0                  // X2 contains the BSS length
1:
    cbz     x1, 2f
    sub     x1, x1, 1
    strb    wzr, [x0,x1]
    b       1b
2:

    // Set up the stack space.

    adrp    x0, {stack}
    add     x0, x0, :lo12:{stack}
    mov     x1, {STACK_COOKIE_LO}           // Lower 16 bits of the stack cookie
    movk    x1, {STACK_COOKIE_HI}, lsl 16   // Higher 16 bits of the stack cookie, keep the lower bits
    str     x1, [x0]                        // Store the stack cookie at the bottom
    add     x0, x0, {STACK_SIZE}            // Stack size
    sub     x0, x0, #8                      // Leave 8 bytes for the stack cookie at the top
    str     x1, [x0]                        // Store the stack cookie at the top
    sub     x0, x0, #8                      // Set the stack pointer
    mov     sp, x0

    // Push `x19` (the IGVM parameter copied from x0) to the stack, its value has to be passed to `start`.

    str     x19, [sp, #-16]!

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

    SMC_CALL_DEBUG_INTERFACE

    // This macro is used to connect to the hypervisor and run the enlightened
    // crash reporting from the handlers in the exception table. Using Rust or stack
    // might be problematic at the time of the crash if the stack is corrupted or the
    // image is not relocated.
    //
    // Save the value to `x5`.
    // Clobbers `x0`-`x6`.
    //
    // N.B. Using the combination of `ldr` and `=` for loading constants is safe w/o
    // relocation as it produces PC-relative addresses or movz/movk instructions if
    // the immediate value fits.

    .macro SET_VP_REGISTER64 vp_reg_name, val
        ldr     x0, =0x100010051            // Input: 1 argument, fast, HvCallSetVpRegisters
        mov     x1, #0xffffffffffffffff     // This partition
        mov     x2, #0xfffffffe             // This VP
        ldr     x3, =\vp_reg_name
        mov     x4, #0                      // Padding
        mov     x6, #0                      // Upper 64 bits of the value
        hvc     #1                          // Hyper-V ABI
    .endm

    .macro EXCEPTION_ENTRY source_and_kind
    .align 7
        // Connect to the hypervisor as can't assume that has been done already.
        // No harm if that is done any number of times.

        mov                 x5, {OHCL_LOADER_OSID}
        SET_VP_REGISTER64   {HV_REGISTER_OSID}

        // Report the crash data

        mov                 x5, \source_and_kind
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P0}
        mrs                 x5, PAR_EL1 
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P1}
        mrs                 x5, ELR_EL1
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P2}
        mrs                 x5, ESR_EL1
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P3}
        mov                 x5, 0 // No message
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_P4}
        mov                 x5, {GUEST_CRASH_CTRL}
        SET_VP_REGISTER64   {HV_REGISTER_GUEST_CRASH_CTRL}

        SMC_CALL_DEBUG_INTERFACE

        // If nothing of the above has made the code break, spin forever.
        b       .
    .endm

    // Vector table must be aligned to a 2KB boundary.
    .balign 0x800
_vector_table_el1:
    // Target and source at same exception level with source SP = SP_EL0
    EXCEPTION_ENTRY 0x00  // Synchronous exception
    EXCEPTION_ENTRY 0x01  // IRQ
    EXCEPTION_ENTRY 0x02  // FIQ
    EXCEPTION_ENTRY 0x03  // SError

    // Target and source at same exception level with source SP = SP_ELx
    EXCEPTION_ENTRY 0x10  // Synchronous exception
    EXCEPTION_ENTRY 0x11  // IRQ
    EXCEPTION_ENTRY 0x12  // FIQ
    EXCEPTION_ENTRY 0x13  // SError

    // Source is at lower exception level running on AArch64
    EXCEPTION_ENTRY 0x20  // Synchronous exception
    EXCEPTION_ENTRY 0x21  // IRQ
    EXCEPTION_ENTRY 0x22  // FIQ
    EXCEPTION_ENTRY 0x23  // SError

    // Source is at lower exception level running on AArch32
    EXCEPTION_ENTRY 0x30  // Synchronous exception
    EXCEPTION_ENTRY 0x31  // IRQ
    EXCEPTION_ENTRY 0x32  // FIQ
    EXCEPTION_ENTRY 0x33  // SError
