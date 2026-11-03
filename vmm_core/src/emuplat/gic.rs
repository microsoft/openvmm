// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to bridge between the `vmotherboard` interrupt controller and a `virt`
//! partition GIC.

use chipset_device_resources::IRQ_LINE_SET;
use std::ops::RangeInclusive;
use std::sync::Arc;
use virt::irqcon::ControlGic;
use vmcore::line_interrupt::LineSetTarget;
use vmotherboard::ChipsetBuilder;

/// Declares persistent GIC line wiring, independent of producer handles.
///
/// `spis` must be a nonempty range of platform-owned SPI INTIDs. Unconnected
/// inputs in this range replay low; MSI allocations must not be included.
pub fn add_gic_line_target(
    chipset: &ChipsetBuilder<'_>,
    irqcon: Arc<dyn ControlGic>,
    spis: RangeInclusive<u32>,
) {
    assert!(!spis.is_empty() && SPI_RANGE.contains(spis.start()) && SPI_RANGE.contains(spis.end()));
    chipset.add_external_line_target(
        IRQ_LINE_SET,
        spis.start() - SPI_RANGE.start()..=spis.end() - SPI_RANGE.start(),
        *spis.start(),
        "gic",
        Arc::new(GicInterruptTarget::new(irqcon)),
    );
}

/// Interrupt target for the GIC.
///
/// Maps platform-owned interrupt lines into GIC interrupt IDs. Register only
/// line-owned SPIs with this target, not the entire SPI namespace: an unwired
/// mapped input is deasserted, including after restore.
pub struct GicInterruptTarget(Arc<dyn ControlGic>);

impl GicInterruptTarget {
    /// Returns a new [`LineSetTarget`].
    pub fn new(irqcon: Arc<dyn ControlGic>) -> Self {
        Self(irqcon)
    }
}

pub const SPI_RANGE: RangeInclusive<u32> = 32..=1019;

/// Fixed platform line inputs, reserved independently of which devices exist.
///
/// IRQ line-set vectors 0 through 31 map to these SPIs. Higher SPIs are used
/// for MSI delivery or explicitly allocated platform lines such as SMMU IRQs.
// TODO: Describe fixed line wiring in platform configuration rather than a range.
pub const PLATFORM_LINE_SPI_RANGE: RangeInclusive<u32> = 32..=63;

impl LineSetTarget for GicInterruptTarget {
    fn set_irq(&self, vector: u32, high: bool) {
        self.0.set_spi_irq(vector, high)
    }

    fn set_unconnected_irq(&self, vector: u32) {
        self.0.set_spi_irq(vector, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use test_with_tracing::test;
    use vmcore::line_interrupt::LineSet;

    #[derive(Default)]
    struct Gic(Mutex<BTreeMap<u32, bool>>);

    impl ControlGic for Gic {
        fn set_spi_irq(&self, irq_id: u32, high: bool) {
            self.0.lock().insert(irq_id, high);
        }
    }

    fn wire(set: &LineSet, gic: &Arc<Gic>) {
        let target = Arc::new(GicInterruptTarget::new(gic.clone()));
        set.add_target(
            0..=PLATFORM_LINE_SPI_RANGE.end() - SPI_RANGE.start(),
            *PLATFORM_LINE_SPI_RANGE.start(),
            "platform",
            target.clone(),
        );
        // A separately allocated platform line, outside the fixed IRQ range.
        set.add_target(868..=868, 900, "SMMU", target);
    }

    #[test]
    fn line_reservations_survive_source_removal_and_reset() {
        for attach_while_running in [false, true] {
            let gic = Arc::new(Gic::default());
            gic.0.lock().insert(64, true);
            let set = LineSet::new();
            let line = set.new_line(0, "device").unwrap();
            let shared = line.new_shared("shared device").unwrap();
            line.set_level(true);
            shared.set_level(true);
            if attach_while_running {
                set.start();
            }
            wire(&set, &gic);
            if !attach_while_running {
                set.start();
            }
            assert!(gic.0.lock()[&32]);
            assert!(!gic.0.lock()[&33]);
            assert!(!gic.0.lock()[&900]);
            set.stop();
            drop(line);
            set.start();
            assert!(gic.0.lock()[&32], "the shared producer is still high");
            set.stop();
            drop(shared);
            assert!(gic.0.lock()[&32], "stopped removal must remain gated");
            set.start();
            assert!(!gic.0.lock()[&32]);
            set.stop();
            set.reset();
            set.start();
            assert!(!gic.0.lock()[&32]);
            assert!(gic.0.lock()[&64], "MSI state is outside the line mappings");
            set.stop();
        }
    }

    #[test]
    fn fresh_restore_deasserts_missing_producers_without_clearing_msis() {
        let source_gic = Arc::new(Gic::default());
        source_gic.0.lock().insert(64, true);
        let source = LineSet::new();
        wire(&source, &source_gic);
        let platform_line = source.new_line(0, "platform device").unwrap();
        let allocated_line = source.new_line(868, "SMMU").unwrap();
        platform_line.set_level(true);
        allocated_line.set_level(true);
        source.start();
        source.stop();
        drop(platform_line);
        drop(allocated_line);

        // Save the controller before stopped deassertions have been replayed.
        let saved = source_gic.0.lock().clone();
        assert!(saved[&32] && saved[&900] && saved[&64]);
        let restored_gic = Arc::new(Gic(Mutex::new(saved)));
        let destination = LineSet::new();
        // Recreate platform wiring, but neither producer. No manager history
        // from the original instance is available.
        wire(&destination, &restored_gic);
        destination.start();
        assert!(!restored_gic.0.lock()[&32]);
        assert!(!restored_gic.0.lock()[&900]);
        assert!(restored_gic.0.lock()[&64]);
        destination.stop();
    }
}
