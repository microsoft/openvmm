// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Naming and fields come from specification for SC16C550B chip.

use bitfield_struct::bitfield;
use inspect::Inspect;
use open_enum::open_enum;

/// 16550 has 16-byte tx and rx FIFOs.
pub const FIFO_SIZE: usize = 16;

open_enum! {
    /// IO port assignments within an 8-register block.
    pub enum Register: u8 {
        RHR = 0, // Receive Holding Register  (RO)
        THR = 0, // Transmit Holding Register (WO)
        DLL = 0, // Divisor Latch LSB         (RW)
        IER = 1, // Interrupt Enable Register (RW)
        DLM = 1, // Divisor Latch MSB         (RW)
        ISR = 2, // Interrupt Status Register (RO)
        FCR = 2, // FIFO Control Register     (WO)
        LCR = 3, // Line Control Register     (RW)
        MCR = 4, // Modem Control Register    (RW)
        LSR = 5, // Line Status Register      (RO)
        RST = 5, // Reset                     (WO)
        MSR = 6, // Modem Status Register     (RO)
        SPR = 7, // Scratch-Pad Register      (RW)
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct InterruptEnableRegister {
    pub received_data_avail: bool,
    pub thr_empty: bool,
    pub receiver_line_status: bool,
    pub modem_status: bool,
    #[bits(4)]
    pub reserved: u8,
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct InterruptIdentificationRegister {
    pub no_interrupt_pending: bool,
    #[bits(3)]
    pub source: u8,
    #[bits(2)]
    pub reserved: u8,
    #[bits(2)]
    pub fifo_state: u8,
}

open_enum! {
    pub enum InterruptSource: u8 {
        MODEM_STATUS = 0,
        THR_EMPTY = 1,
        RECEIVED_DATA_AVAIL = 2,
        RECEIVER_LINE_STATUS = 3,
        RECEIVE_TIMEOUT = 6,
    }
}

open_enum! {
    pub enum FifoState: u8 {
        DISABLED = 0,
        ENABLED = 3,
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct FifoControlRegister {
    pub enable_fifos: bool,
    pub clear_rx_fifo: bool,
    pub clear_tx_fifo: bool,
    #[bits(1)]
    pub dma_mode: u8,
    #[bits(2)]
    pub reserved: u8,
    #[bits(2)]
    pub rx_fifo_int_trigger: u8,
}

open_enum! {
    pub enum RxFifoInterruptTrigger: u8 {
        BYTES_1 = 0,
        BYTES_4 = 1,
        BYTES_8 = 2,
        BYTES_14 = 3,
    }
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct LineControlRegister {
    #[bits(2)]
    pub data_word_length: u8,
    #[bits(1)]
    pub stop_bits: u8,
    #[bits(3)]
    pub parity: u8,
    pub break_enabled: bool,
    pub dlab: bool,
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct ModemControlRegister {
    pub dtr: bool, // Data Terminal Ready
    pub rts: bool, // Request To Send
    pub out1: bool,
    pub out2: bool,
    pub loopback: bool,
    #[bits(3)]
    pub reserved: u8,
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct LineStatusRegister {
    pub rx_ready: bool,
    pub overrun_error: bool,
    pub parity_error: bool,
    pub framing_error: bool,
    pub break_signal_received: bool,
    pub thr_empty: bool,
    pub thr_and_tsr_empty: bool,
    pub fifo_data_error: bool,
}

#[derive(Inspect)]
#[bitfield(u8)]
pub struct ModemStatusRegister {
    pub cts_change: bool,
    pub dsr_change: bool,
    pub ri_went_low: bool,
    pub dcd_change: bool,
    pub cts: bool, // Clear To Send
    pub dsr: bool, // Data Set Ready
    pub ri: bool,  // Ring Indicator
    pub dcd: bool, // Data Carrier Detect
}
