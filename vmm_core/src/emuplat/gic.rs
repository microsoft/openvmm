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

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use test_with_tracing::test;
    use vmcore::line_interrupt::LineSet;

    #[derive(Default)]
    struct Gic(Mutex<Vec<(u32, bool)>>);

    impl ControlGic for Gic {
        fn set_spi_irq(&self, irq_id: u32, high: bool) {
            self.0.lock().push((irq_id, high));
        }
    }

    #[test]
    fn stopped_source_removal_withdraws_only_previously_delivered_assertions() {
        for attach_while_running in [false, true] {
            let gic = Arc::new(Gic::default());
            let set = LineSet::new();
            let line = set.new_line(0, "device").unwrap();
            line.set_level(true);
            if attach_while_running {
                set.start();
            }
            set.add_target(
                0..=1,
                32,
                "GIC",
                Arc::new(GicInterruptTarget::new(gic.clone())),
            );
            if !attach_while_running {
                set.start();
            }
            assert_eq!(*gic.0.lock(), [(32, true)]);
            set.stop();
            drop(line);
            assert_eq!(*gic.0.lock(), [(32, true)]);

            // This target never received the managed high, even though it
            // attaches to the same wire before replay.
            let late_gic = Arc::new(Gic::default());
            set.add_target(
                0..=1,
                32,
                "late GIC",
                Arc::new(GicInterruptTarget::new(late_gic.clone())),
            );
            set.start();
            assert_eq!(*gic.0.lock(), [(32, true), (32, false)]);
            assert!(late_gic.0.lock().is_empty());
            set.stop();
            set.start();
            assert_eq!(*gic.0.lock(), [(32, true), (32, false)]);
            assert!(late_gic.0.lock().is_empty());
            set.stop();
        }
    }

    #[test]
    fn reset_discards_old_managed_assertions() {
        let gic = Arc::new(Gic::default());
        let set = LineSet::new();
        set.add_target(
            0..=0,
            32,
            "GIC",
            Arc::new(GicInterruptTarget::new(gic.clone())),
        );
        let line = set.new_line(0, "device").unwrap();
        line.set_level(true);
        set.start();
        set.stop();
        drop(line);
        set.reset();
        gic.0.lock().clear();
        // State restored independently into an unwired SPI is not ours to clear.
        set.start();
        assert!(gic.0.lock().is_empty());
        set.stop();
    }

    #[test]
    fn managed_gic_replays_connected_inputs_without_clearing_unconnected_spis() {
        for attach_while_running in [false, true] {
            let gic = Arc::new(Gic::default());
            let set = LineSet::new();
            let line = set.new_line(0, "device").unwrap();
            let removed = set.new_line(1, "removed device").unwrap();
            drop(removed);
            line.set_level(true);
            if attach_while_running {
                set.start();
            }
            set.add_target(
                0..=2,
                32,
                "GIC",
                Arc::new(GicInterruptTarget::new(gic.clone())),
            );
            if !attach_while_running {
                assert!(gic.0.lock().is_empty());
                set.start();
            }
            assert_eq!(*gic.0.lock(), [(32, true)]);
            set.stop();
            line.set_level(false);
            line.set_level(true);
            line.set_level(false);
            assert_eq!(*gic.0.lock(), [(32, true)]);
            set.start();
            assert_eq!(*gic.0.lock(), [(32, true), (32, false)]);
        }
    }
}
