// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Interrupt handling for devices.

use std::ops::RangeInclusive;

/// A device that can act as a target for a line interrupt.
pub trait LineInterruptTarget {
    /// Set an interrupt line state.
    fn set_irq(&mut self, vector: u32, high: bool);

    /// Returns the valid vector ranges for this target.
    fn valid_lines(&self) -> &[RangeInclusive<u32>];
}

/// A device that can handle an EOI initiated from a processor-specific
/// interrupt controller interface.
pub trait HandleEoi {
    /// EOI the interrupt.
    fn handle_eoi(&mut self, vector: u32);
}

/// A device that can handle an x86 PIC interrupt request.
pub trait AcknowledgePicInterrupt {
    /// Gets the current pending IRQ and sets it in service.
    fn acknowledge_interrupt(&mut self) -> Option<u8>;
}
