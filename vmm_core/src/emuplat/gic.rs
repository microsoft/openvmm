// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to bridge between the `vmotherboard` interrupt controller and a `virt`
//! partition GIC.

use std::ops::RangeInclusive;
use std::sync::Arc;
use virt::irqcon::ControlGic;
use vmcore::line_interrupt::LineSetTarget;

/// Interrupt target for the GIC.
///
/// Maps the interrupt lines into GIC interrupt IDs. Only SPIs (starting at
/// IRQ 32) are supported.
pub struct GicInterruptTarget(Arc<dyn ControlGic>);

impl GicInterruptTarget {
    /// Returns a new [`LineSetTarget`].
    pub fn new(irqcon: Arc<dyn ControlGic>) -> Self {
        Self(irqcon)
    }
}

pub const SPI_RANGE: RangeInclusive<u32> = 32..=1019;

impl LineSetTarget for GicInterruptTarget {
    fn set_irq(&self, vector: u32, high: bool) {
        self.0.set_spi_irq(vector, high)
    }
}
