// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Port I/O primitives for x86_64.

use core::arch::asm;

/// Write a byte to an x86 I/O port.
///
/// # Safety Considerations
///
/// This is a safe wrapper around an `out` instruction. The caller must
/// ensure that `port` addresses a valid device register and that writing
/// to it will not cause unintended side effects.
pub fn outb(port: u16, data: u8) {
    // SAFETY: This executes an `out` instruction which requires CPL 0
    // or IOPL >= CPL. OpenTMK runs as a UEFI application at ring 0
    // (or under a hypervisor that grants I/O port access). The caller
    // is responsible for ensuring `port` addresses a valid device
    // register.
    unsafe {
        asm! {
            "out dx, al",
            in("dx") port,
            in("al") data,
        }
    }
}

/// Read a byte from an x86 I/O port.
///
/// # Safety Considerations
///
/// This is a safe wrapper around an `in` instruction. The caller must
/// ensure that `port` addresses a valid device register and that reading
/// from it will not cause unintended side effects.
pub fn inb(port: u16) -> u8 {
    let mut data;
    // SAFETY: See `outb` -- same privilege requirements apply. The `in`
    // instruction reads from the specified I/O port into `al`.
    unsafe {
        asm! {
            "in al, dx",
            in("dx") port,
            out("al") data,
        }
    }
    data
}

/// Read a 16-bit word from an x86 I/O port.
///
/// # Safety Considerations
///
/// This is a safe wrapper around an `in` instruction. The caller must
/// ensure that `port` addresses a valid device register and that reading
/// from it will not cause unintended side effects.
pub fn inw(port: u16) -> u16 {
    let mut data;
    // SAFETY: See `outb` -- same privilege requirements apply. The `in`
    // instruction reads a word from the specified I/O port into `ax`.
    unsafe {
        asm! {
            "in ax, dx",
            in("dx") port,
            out("ax") data,
        }
    }
    data
}

/// Write a 16-bit word to an x86 I/O port.
///
/// # Safety Considerations
///
/// This is a safe wrapper around an `out` instruction. The caller must
/// ensure that `port` addresses a valid device register and that writing
/// to it will not cause unintended side effects.
pub fn outw(port: u16, data: u16) {
    // SAFETY: See `outb` -- same privilege requirements apply. The `out`
    // instruction writes a word from `ax` to the specified I/O port.
    unsafe {
        asm! {
            "out dx, ax",
            in("dx") port,
            in("ax") data,
        }
    }
}

/// Read a 32-bit double word from an x86 I/O port.
///
/// # Safety Considerations
///
/// This is a safe wrapper around an `in` instruction. The caller must
/// ensure that `port` addresses a valid device register and that reading
/// from it will not cause unintended side effects.
pub fn inl(port: u16) -> u32 {
    let mut data;
    // SAFETY: See `outb` -- same privilege requirements apply. The `in`
    // instruction reads a dword from the specified I/O port into `eax`.
    unsafe {
        asm! {
            "in eax, dx",
            in("dx") port,
            out("eax") data,
        }
    }
    data
}

/// Write a 32-bit double word to an x86 I/O port.
///
/// # Safety Considerations
///
/// This is a safe wrapper around an `out` instruction. The caller must
/// ensure that `port` addresses a valid device register and that writing
/// to it will not cause unintended side effects.
pub fn outl(port: u16, data: u32) {
    // SAFETY: See `outb` -- same privilege requirements apply. The `out`
    // instruction writes a dword from `eax` to the specified I/O port.
    unsafe {
        asm! {
            "out dx, eax",
            in("dx") port,
            in("eax") data,
        }
    }
}
