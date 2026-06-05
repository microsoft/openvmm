// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! aarch64 MMIO-based serial port, UART PL011.
//!
//! Used for debug output. Follows
//! [PrimeCell UART (PL011) Technical Reference Manual](https://developer.arm.com/documentation/ddi0183/g/)
//!
//! PL011 Registers:
//!
//! Offset  Name              Type Reset        Bits    Description
//! ----------------------------------------------------------------------
//! 0x000   UARTDR            RW   0x---        12/8    Data Register
//! 0x004   UARTRSR/UARTECR   RW   0x0          4/0     Receive Status Register/Error Clear Register
//! 0x018   UARTFR            RO   0b-10010---  9       Flag Register
//! 0x020   UARTILPR          RW   0x00         8       IrDA Low-Power Counter Register
//! 0x024   UARTIBRD          RW   0x0000       16      Integer Baud Rate Register
//! 0x028   UARTFBRD          RW   0x00         6       Fractional Baud Rate Register
//! 0x02C   UARTLCR_H         RW   0x00         8       Line Control Register
//! 0x030   UARTCR            RW   0x0300       16      Control Register
//! 0x034   UARTIFLS          RW   0x12         6       Interrupt FIFO Level Select Register
//! 0x038   UARTIMSC          RW   0x000        11      Interrupt Mask Set/Clear Register
//! 0x03C   UARTRIS           RO   0x00-        11      Raw Interrupt Status Register
//! 0x040   UARTMIS           RO   0x00-        11      Masked Interrupt Status Register
//! 0x044   UARTICR           WO   -            11      Interrupt Clear Register
//! 0x048   UARTDMACR         RW   0x00         3       DMA Control Register
//! 0xFE0   UARTPeriphID0     RO   0x11         8       UARTPeriphID0 Register
//! 0xFE4   UARTPeriphID1     RO   0x10         8       UARTPeriphID1 Register
//! 0xFE8   UARTPeriphID2     RO   0x_4a        8       UARTPeriphID2 Register
//! 0xFEC   UARTPeriphID3     RO   0x00         8       UARTPeriphID3 Register
//! 0xFF0   UARTPCellID0      RO   0x0D         8       UARTPCellID0 Register
//! 0xFF4   UARTPCellID1      RO   0xF0         8       UARTPCellID1 Register
//! 0xFF8   UARTPCellID2      RO   0x05         8       UARTPCellID2 Register
//! 0xFFC   UARTPCellID3      RO   0xB1         8       UARTPCellID3 Register

#![expect(dead_code)]

use core::hint::spin_loop;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering;

#[derive(Debug, Clone, Copy)]
#[repr(u16)]
enum Pl011Register {
    /// Data Register
    Dr = 0x000,
    /// Receive Status Register/Error Clear Register
    RsrOrEcr = 0x004,
    /// Flag register
    Fr = 0x018,
    /// Integer Baud Rate Register
    Ibrd = 0x024,
    /// Fractional Baud Rate Register
    Fbrd = 0x028,
    /// Line Control Register
    LcrHigh = 0x02c,
    /// Control Register
    Cr = 0x030,
    /// Masked Interrupt Status Register
    Imsc = 0x038,
    /// Interrupt Clear Register
    Icr = 0x044,
    /// DMA Control Register
    DmaCr = 0x048,
    /// UARTPeriphID0 Register
    PeriphID0 = 0xFE0,
    /// UARTPeriphID1 Register
    PeriphID1 = 0xFE4,
    /// UARTPeriphID2 Register
    PeriphID2 = 0xFE8,
    /// UARTPeriphID3 Register
    PeriphID3 = 0xFEC,
    /// UARTPCellID0 Register
    PCellID0 = 0xFF0,
    /// UARTPCellID1 Register
    PCellID1 = 0xFF4,
    /// UARTPCellID2 Register
    PCellID2 = 0xFF8,
    /// UARTPCellID3 Register
    PCellID3 = 0xFFC,
}

const CR_RX_ENABLE: u32 = 0x200;
const CR_TX_ENABLE: u32 = 0x100;
const CR_UART_ENABLE: u32 = 1;
const LCR_H_FIFO_EN: u32 = 0x10;
const LCR_H_8BITS: u32 = 0x60;

const _FR_TX_EMPTY: u32 = 0x080;
const _FR_RX_FULL: u32 = 0x040;
const FR_TX_FULL: u32 = 0x020;
const _FR_RX_EMPTY: u32 = 0x010;
const FR_BUSY: u32 = 0x008;

/// The Hyper-V PL011 host emulated PL011's are found at these
/// base addresses. Should come from ACPI or DT of course yet
/// due to having been hardcoded in some products makes that
/// virtually constants.
const PL011_HYPER_V_BASE_1: u64 = 0xeffec000;
const _PL011_HYPER_V_BASE_2: u64 = 0xeffeb000;
const PL011_BASE: u64 = PL011_HYPER_V_BASE_1;

fn read_register(reg: Pl011Register) -> u32 {
    // SAFETY: using the PL011 MMIO address.
    unsafe { core::ptr::read_volatile((PL011_BASE + reg as u64) as *const u32) }
}

fn write_register(reg: Pl011Register, val: u32) {
    // SAFETY: using the PL011 MMIO address.
    unsafe {
        core::ptr::write_volatile((PL011_BASE + reg as u64) as *mut u32, val);
    }
}

fn cell_id() -> u32 {
    // This can easily be rewritten employing
    // bare arithmetic yet the compiler does a very good job
    // so using the domain abstractions.
    [
        Pl011Register::PCellID3,
        Pl011Register::PCellID2,
        Pl011Register::PCellID1,
        Pl011Register::PCellID0,
    ]
    .iter()
    .fold(0, |id_running, &r| {
        id_running.wrapping_shl(8) | (read_register(r) as u8 as u32)
    })
}

fn periph_id() -> u32 {
    // This can easily be rewritten employing
    // bare arithmetic yet the compiler does a very good job
    // so using the domain abstractions.
    [
        Pl011Register::PeriphID3,
        Pl011Register::PeriphID2,
        Pl011Register::PeriphID1,
        Pl011Register::PeriphID0,
    ]
    .iter()
    .fold(0, |id_running, &r| {
        id_running.wrapping_shl(8) | (read_register(r) as u8 as u32)
    })
}

fn poll_tx_not_full() {
    while read_register(Pl011Register::Fr) & FR_TX_FULL != 0 {
        spin_loop();
    }
}

fn poll_not_busy() {
    while read_register(Pl011Register::Fr) & FR_BUSY != 0 {
        spin_loop();
    }
}

/// Disables the functional parts of the UART, drains FIFOs,
/// sets baud rate and enables the UART in the polling mode.
/// Might be geared towards the real hardware more than the virtual one.
/// Works with qemu and Hyper-V.
fn reset_and_init() {
    // Mask interrupts (lower 11 bits)
    write_register(Pl011Register::Imsc, 0x7ff);
    // Clear interrupts (lower 11 bits)
    write_register(Pl011Register::Icr, 0x7ff);
    // Disable DMA on Rx and Tx
    write_register(Pl011Register::DmaCr, 0x0);

    // Leave Rx and Tx enabled to drain FIFOs.
    write_register(Pl011Register::Cr, CR_RX_ENABLE | CR_TX_ENABLE);
    read_register(Pl011Register::Cr); // wait
    read_register(Pl011Register::Cr); // wait
    poll_not_busy();

    // Disable Rx, Tx, and UART.
    write_register(Pl011Register::Cr, 0x00000000);

    // Set integer and fractional parts of the baud rate,
    // hardcoded for now
    write_register(Pl011Register::Fbrd, 0x00000004);
    write_register(Pl011Register::Ibrd, 0x00000027);
    // The UARTLCR_H, UARTIBRD, and UARTFBRD registers form the single 30-bit
    // wide UARTLCR Register that is updated on a single write strobe generated by a
    // UARTLCR_H write
    write_register(Pl011Register::LcrHigh, LCR_H_FIFO_EN | LCR_H_8BITS);

    // Clear the errors
    write_register(Pl011Register::RsrOrEcr, 0);

    // Enable Tx and Rx
    write_register(Pl011Register::Cr, CR_RX_ENABLE | CR_TX_ENABLE);
    read_register(Pl011Register::Cr); // wait
    read_register(Pl011Register::Cr); // wait
    poll_not_busy();

    // Enable UART
    write_register(
        Pl011Register::Cr,
        CR_RX_ENABLE | CR_TX_ENABLE | CR_UART_ENABLE,
    );
    poll_not_busy();
}

/// A PL011 serial port.
pub struct Serial;

static SUPPORTED: AtomicBool = AtomicBool::new(false);

impl Serial {
    /// Initializes the serial port.
    pub fn init() -> Serial {
        const SUPPORTED_PL011_CELLS: &[u32] = &[0xB105_F00D];

        let cell_id = cell_id();
        let supported = SUPPORTED_PL011_CELLS.contains(&cell_id);
        if supported {
            reset_and_init();
        }
        SUPPORTED.store(supported, Ordering::Relaxed);

        Self
    }
}

impl core::fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if !SUPPORTED.load(Ordering::Relaxed) {
            return Ok(());
        }

        for byte in s.bytes() {
            poll_tx_not_full();
            write_register(Pl011Register::Dr, byte.into());
        }

        Ok(())
    }
}
