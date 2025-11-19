// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to bridge between the `vmotherboard` interrupt controller and a `virt`
//! partition APIC.

use hvdef::Vtl;
use std::sync::Arc;
use virt::X86Partition;
use vm_topology::processor::VpIndex;
use vmcore::line_interrupt::LineSetTarget;

/// A [`LineSetTarget`] implementation that raises APIC local interrupt lines.
pub struct ApicLintLineTarget<T: X86Partition> {
    partition: Arc<T>,
    vtl: Vtl,
}

impl<T: X86Partition> ApicLintLineTarget<T> {
    /// Creates a new APIC LINT line set target.
    pub fn new(partition: Arc<T>, vtl: Vtl) -> Self {
        Self { partition, vtl }
    }
}

impl<T: X86Partition> LineSetTarget for ApicLintLineTarget<T> {
    fn set_irq(&self, vector: u32, high: bool) {
        if !high {
            return;
        }
        let vp_index = VpIndex::new(vector / 2);
        let lint = vector % 2;
        self.partition.pulse_lint(vp_index, self.vtl, lint as u8);
    }
}
