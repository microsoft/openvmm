// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed register contract for the Hyper-V-compatible x86 chipset.
//!
//! Kept here so both the device models and freestanding ACPI builders can
//! use the same values without a hosted device dependency.

/// Default base of the power management PIO block.
pub const DEFAULT_PM_PIO_BASE: u16 = 0x400;
/// Default system control interrupt.
pub const DEFAULT_ACPI_IRQ: u32 = 9;
/// PM1 status register offset; the enable register follows at offset 2.
pub const PM_STATUS: u16 = 0;
/// PM1 control register offset.
pub const PM_CONTROL: u16 = 4;
/// 32-bit PM timer register offset.
pub const PM_TIMER: u16 = 8;
/// GPE0 status register offset; the enable register follows at offset 0xe.
pub const PM_GPE0_STATUS: u16 = 0xc;
/// Reset register offset.
pub const PM_RESET: u16 = 0x33;
/// Value written to the reset register to restart the VM.
pub const RESET_VALUE: u8 = 1;
/// Base of the default IOAPIC MMIO page.
pub const IOAPIC_BASE_ADDRESS: u32 = 0xfec0_0000;
