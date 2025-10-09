// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::arch::asm;

/// Write a byte to a port.
pub fn outb(port: u16, data: u8) {
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "out dx, al",
            in("dx") port,
            in("al") data,
        }
    }
}

/// Read a byte from a port.
pub fn inb(port: u16) -> u8 {
    let mut data;
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "in al, dx",
            in("dx") port,
            out("al") data,
        }
    }
    data
}

/// Read a double word from a port.
pub fn inl(port: u16) -> u32 {
    let mut data;
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "in eax, dx",
            in("dx") port,
            out("eax") data,
        }
    }
    data
}

/// Write a double word to a port.
/// This is a no-op on x86.
pub fn outl(port: u16, data: u32) {
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "out dx, eax",
            in("dx") port,
            in("eax") data,
        }
    }
}
