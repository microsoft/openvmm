// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::arch::asm;

/// Write a byte to a port.
///
/// # Safety
/// Caller should assume that the port being written to is safe to do so
pub unsafe fn outb(port: u16, data: u8) {
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
///
/// # Safety
/// Caller should assume that the port being read from is safe to do so
pub unsafe fn inb(port: u16) -> u8 {
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

/// Read a word from a port.
///
/// # Safety
/// Caller should assume that the port being read from is safe to do so
pub unsafe fn inh(port: u16) -> u16 {
    let mut data;
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "in ax, dx",
            in("dx") port,
            out("ax") data,
        }
    }
    data
}

/// Write a word to a port.
///
/// # Safety
/// Caller should assume that the port being written to is safe to do so
pub unsafe fn outh(port: u16, data: u16) {
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "out dx, ax",
            in("dx") port,
            in("ax") data,
        }
    }
}

/// Read a double word from a port.
///
/// # Safety
/// Caller should assume that the port being read from is safe to do so
pub unsafe fn inl(port: u16) -> u32 {
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
///
/// # Safety
/// Caller should assume that the port being written to is safe to do so
pub unsafe fn outl(port: u16, data: u32) {
    // SAFETY: The caller has assured us this is safe.
    unsafe {
        asm! {
            "out dx, eax",
            in("dx") port,
            in("eax") data,
        }
    }
}
