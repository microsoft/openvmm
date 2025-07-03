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