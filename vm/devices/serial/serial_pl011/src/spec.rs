// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Naming and fields come from specification for PL011 chip.
//! <https://developer.arm.com/documentation/ddi0183/g/?lang=en>

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;

/// PL011 has 32-byte FIFOs.
pub const FIFO_SIZE: usize = 32;

open_enum! {
    /// MMIO port assignments
    pub enum Register: u16 {
        UARTDR         = 0x000, // Data Register. 12 bit (R), 8 bit (W)
        UARTRSR        = 0x004, // Receive Status Register (R)
        UARTECR        = 0x004, // Error Clear Register
        UARTFR         = 0x018, // Flag Register. Read-only
        UARTILPR       = 0x020, // IrDA Low-Power Counter Register
        UARTIBRD       = 0x024, // Integer Baud Rate Register
        UARTFBRD       = 0x028, // Fractional Baud Rate Register
        UARTLCR_H      = 0x02C, // Line Control Register
        UARTCR         = 0x030, // Control Register
        UARTIFLS       = 0x034, // Interrupt FIFO Level Select Register
        UARTIMSC       = 0x038, // Interrupt Mask Set/Clear Register
        UARTRIS        = 0x03C, // Raw Interrupt Status Register
        UARTMIS        = 0x040, // Masked Interrupt Status Register
        UARTICR        = 0x044, // Interrupt Clear Register
        UARTDMACR      = 0x048, // DMA Control Register

        // Offsets 0x04C - 0xFDC are reserved

        UARTPERIPHID0  = 0xFE0, // Peripheral Identification Register 0
        UARTPERIPHID1  = 0xFE4, // Peripheral Identification Register 1
        UARTPERIPHID2  = 0xFE8, // Peripheral Identification Register 2
        UARTPERIPHID3  = 0xFEC, // Peripheral Identification Register 3
        UARTPCELLID0   = 0xFF0, // PrimeCell Identification Register 0
        UARTPCELLID1   = 0xFF4, // PrimeCell Identification Register 1
        UARTPCELLID2   = 0xFF8, // PrimeCell Identification Register 2
        UARTPCELLID3   = 0xFFC, // PrimeCell Identification Register 3
    }
}

pub const REGISTERS_SIZE: u64 = 0x1000;

open_enum! {
    pub enum FifoLevelSelect: u8 {
        BYTES_4 = 0,
        BYTES_8 = 1,
        BYTES_16 = 2,
        BYTES_24 = 3,
        BYTES_28 = 4,
    }
}

#[derive(Inspect)]
#[bitfield(u16)]
pub struct FlagRegister {
    pub cts: bool,  // UARTFR_CTS   = 0x0001; Clear to send
    pub dsr: bool,  // UARTFR_DSR   = 0x0002; Data set ready
    pub dcd: bool,  // UARTFR_DCD   = 0x0004; Data carrier detect
    pub busy: bool, // UARTFR_BUSY  = 0x0008; UART busy
    pub rxfe: bool, // UARTFR_RXFE  = 0x0010; Receive FIFO empty
    pub txff: bool, // UARTFR_TXFF  = 0x0020; Transmit FIFO full
    pub rxff: bool, // UARTFR_RXFF  = 0x0040; Receive FIFO full
    pub txfe: bool, // UARTFR_TXFE  = 0x0080; Transmit FIFO empty
    pub ri: bool,   // UARTFR_RI    = 0x0100; Ring indicator
    #[bits(7)]
    reserved: u8,
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct LineControlRegister {
    #[bits(4)]
    unused: u8,
    pub enable_fifos: bool, // UARTLCR_H_FIFO_ENABLE_MASK = 0x10;
    #[bits(3)]
    unused2: u8,
}

#[derive(Inspect)]
#[bitfield(u16)]
pub struct ControlRegister {
    pub enabled: bool, // UARTCR_UARTEN = 0x0001; UART enable
    #[bits(2)]
    unused: u8,
    #[bits(4)]
    reserved: u8,
    pub loopback: bool, // UARTCR_LBE   = 0x0080; Loopback enable
    pub txe: bool,      // UARTCR_TXE   = 0x0100; Transmit enable
    pub rxe: bool,      // UARTCR_RXE   = 0x0200; Receive enable
    pub dtr: bool,      // UARTCR_DTR   = 0x0400; Data transmit ready
    pub rts: bool,      // UARTCR_RTS   = 0x0800; Request to send
    #[bits(4)]
    unused2: u8,
}

impl ControlRegister {
    pub fn clear_reserved(&mut self) -> ControlRegister {
        self.with_reserved(0)
    }
}

#[derive(Inspect)]
#[bitfield(u16)]
pub struct InterruptRegister {
    pub ri: bool,  // UARTRI    = 0x0001; RI modem interrupt status
    pub cts: bool, // UARTCTS   = 0x0002; CTS modem interrupt status
    pub dcd: bool, // UARTDCD   = 0x0004; DCD modem interrupt status
    pub dsr: bool, // UARTDSR   = 0x0008; DSR modem interrupt status
    pub rx: bool,  // RXRIS     = 0x0010; Receive interrupt status
    pub tx: bool,  // TXRIS     = 0x0020; Transmit interrupt status
    pub rt: bool,  // RTRIS     = 0x0040; Receive timeout interrupt status
    pub fe: bool,  // FERIS     = 0x0080; Framing error interrupt status
    pub pe: bool,  // PERIS     = 0x0100; Parity error interrupt status
    pub be: bool,  // BERIS     = 0x0200; Break error interrupt status
    pub oe: bool,  // OERIS     = 0x0400; Overrun error interrupt status
    #[bits(5)]
    reserved: u8,
}

impl InterruptRegister {
    pub fn clear_reserved(&mut self) -> InterruptRegister {
        self.with_reserved(0)
    }
}

#[derive(Inspect)]
#[bitfield(u16)]
pub struct InterruptFifoLevelSelectRegister {
    #[bits(3)]
    pub txiflsel: u8,
    #[bits(3)]
    pub rxiflsel: u8,
    #[bits(10)]
    reserved: u16,
}

impl InterruptFifoLevelSelectRegister {
    pub fn clear_reserved(&mut self) -> InterruptFifoLevelSelectRegister {
        self.with_reserved(0)
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct FractionalBaudRateRegister {
    #[bits(6)]
    pub baud: u8,
    #[bits(2)]
    reserved: u8,
}

impl FractionalBaudRateRegister {
    pub fn clear_reserved(&mut self) -> FractionalBaudRateRegister {
        self.with_reserved(0)
    }
}

#[derive(Inspect)]
#[bitfield(u16)]
pub struct DmaControlRegister {
    #[bits(3)]
    pub valid: u8,
    #[bits(13)]
    reserved: u16,
}

impl DmaControlRegister {
    pub fn clear_reserved(&mut self) -> DmaControlRegister {
        self.with_reserved(0)
    }
}

// Spec-defined defaults:
// It's used to read a specific register offset (UARTPERIPHID0..3) that identify the device.
pub const UARTPERIPH_ID: [u16; 4] = [0x11, 0x10, 0x34, 0x00];
// It's used to read a specific register offset (UARTPCELLID0..3) that identify the device.
pub const UARTPCELL_ID: [u16; 4] = [0x0D, 0xF0, 0x05, 0xB1];
