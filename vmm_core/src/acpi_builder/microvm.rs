// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM MADT additions: interrupt source overrides that describe legacy
//! IRQs as active-high and level-triggered.

use super::AcpiArchConfig;
use acpi_spec::madt::InterruptPolarity;
use acpi_spec::madt::InterruptTriggerMode;
use zerocopy::IntoBytes;

/// Appends an active-high, level-triggered interrupt source override for each
/// of the x86 `level_triggered_irqs` to the MADT entries in `madt_extra`.
pub(super) fn extend_madt_level_triggered_irqs(arch: &AcpiArchConfig, madt_extra: &mut Vec<u8>) {
    if let AcpiArchConfig::X86 {
        level_triggered_irqs,
        ..
    } = *arch
    {
        for &irq in level_triggered_irqs {
            madt_extra.extend_from_slice(
                acpi_spec::madt::MadtInterruptSourceOverride::new(
                    irq.try_into().expect("legacy IRQ should be in range"),
                    irq,
                    Some(InterruptPolarity::ActiveHigh),
                    Some(InterruptTriggerMode::Level),
                )
                .as_bytes(),
            );
        }
    }
}
