// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "aarch64")]

//! GIC SPI layout resolver for aarch64 VMs.
//!
//! This module determines the GIC SPI assignments for all platform devices
//! that need dynamically allocated interrupts. It is the SPI analogue of
//! [`super::memory_layout`]: all allocations happen in a single deterministic
//! pass so that the assignments are a pure function of the VM configuration.
//! This is critical for hibernation — a resumed VM must get the same SPI
//! layout as the original.
//!
//! SPIs are allocated top-down from the highest SPI supported by the GIC
//! (determined by `gic_nr_irqs`). This maximizes distance from the guest-side
//! vPCI MSI allocator (Hyper-V PCI driver in Linux), which allocates bottom-up
//! starting at INTID 64.

/// Top-down GIC SPI allocator.
struct SpiAllocator {
    range_start: u32,
    /// One past the last allocated INTID, or `range_end + 1` when nothing
    /// has been allocated yet.
    cursor: u32,
}

impl SpiAllocator {
    fn new(range: std::ops::RangeInclusive<u32>) -> Self {
        Self {
            range_start: *range.start(),
            cursor: *range.end() + 1,
        }
    }

    /// Allocates a single SPI, returning its GIC INTID.
    fn alloc(&mut self, tag: &str) -> anyhow::Result<u32> {
        if self.cursor <= self.range_start {
            anyhow::bail!("SPI exhausted allocating {tag}");
        }
        self.cursor -= 1;
        Ok(self.cursor)
    }

    /// Allocates a contiguous block of `count` SPIs, returning the lowest
    /// GIC INTID in the block.
    fn alloc_block(&mut self, tag: &str, count: u32) -> anyhow::Result<u32> {
        let available = self.cursor - self.range_start;
        if count > available {
            anyhow::bail!(
                "SPI exhausted allocating {tag}: need {count}, only {available} remaining"
            );
        }
        self.cursor -= count;
        Ok(self.cursor)
    }
}

/// Inputs to the SPI layout resolver.
pub(super) struct SpiLayoutInput {
    /// Total number of GIC interrupt lines (INTIDs 0..gic_nr_irqs-1).
    /// Determines the highest usable SPI.
    pub gic_nr_irqs: u32,
    /// Number of SPIs to reserve for GICv2m MSI delivery. `None` when using
    /// ITS (no v2m block needed).
    pub v2m_spi_count: Option<u32>,
    /// Number of SMMUv3 instances. Each instance gets two SPIs (event queue
    /// and global error).
    pub smmu_count: usize,
}

/// Resolved SPI assignments for all platform devices.
pub(super) struct ResolvedSpiLayout {
    /// GICv2m SPI base INTID. `None` when using ITS.
    pub v2m_spi_base: Option<u32>,
    /// Per-SMMU SPI assignments, one entry per instance.
    pub smmu: Vec<SmmuSpiAllocation>,
}

/// Allocated SPI pair for a single SMMUv3 instance.
pub(super) struct SmmuSpiAllocation {
    /// GIC INTID for the event queue interrupt.
    pub evtq_intid: u32,
    /// GIC INTID for the global error interrupt.
    pub gerr_intid: u32,
}

/// Resolves SPI assignments for all platform devices.
///
/// All allocations happen here in a single top-down pass over the SPI range
/// `[64, gic_nr_irqs-1]`. The order of allocations determines the layout and
/// must not change across OpenVMM versions for a given config, or hibernation
/// will break.
pub(super) fn resolve_spi_layout(input: &SpiLayoutInput) -> anyhow::Result<ResolvedSpiLayout> {
    let max_intid = input.gic_nr_irqs.saturating_sub(1).min(1019);
    let mut spi = SpiAllocator::new(64..=max_intid);

    // --- Allocation order (do not reorder!) ---

    // 1. GICv2m MSI block.
    let v2m_spi_base = input
        .v2m_spi_count
        .map(|count| spi.alloc_block("gicv2m", count))
        .transpose()?;

    // 2. SMMU instance SPIs (2 per instance: evtq + gerror).
    let mut smmu = Vec::with_capacity(input.smmu_count);
    for idx in 0..input.smmu_count {
        smmu.push(SmmuSpiAllocation {
            evtq_intid: spi.alloc(&format!("smmu{idx}-evtq"))?,
            gerr_intid: spi.alloc(&format!("smmu{idx}-gerr"))?,
        });
    }

    Ok(ResolvedSpiLayout { v2m_spi_base, smmu })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2m_then_smmu() {
        let result = resolve_spi_layout(&SpiLayoutInput {
            gic_nr_irqs: 992,
            v2m_spi_count: Some(64),
            smmu_count: 2,
        })
        .unwrap();

        assert_eq!(result.v2m_spi_base, Some(928));
        assert_eq!(result.smmu[0].evtq_intid, 927);
        assert_eq!(result.smmu[0].gerr_intid, 926);
        assert_eq!(result.smmu[1].evtq_intid, 925);
        assert_eq!(result.smmu[1].gerr_intid, 924);
    }

    #[test]
    fn its_skips_v2m() {
        let result = resolve_spi_layout(&SpiLayoutInput {
            gic_nr_irqs: 992,
            v2m_spi_count: None,
            smmu_count: 1,
        })
        .unwrap();

        assert_eq!(result.v2m_spi_base, None);
        assert_eq!(result.smmu[0].evtq_intid, 991);
        assert_eq!(result.smmu[0].gerr_intid, 990);
    }

    #[test]
    fn no_devices() {
        let result = resolve_spi_layout(&SpiLayoutInput {
            gic_nr_irqs: 992,
            v2m_spi_count: None,
            smmu_count: 0,
        })
        .unwrap();

        assert_eq!(result.v2m_spi_base, None);
        assert!(result.smmu.is_empty());
    }
}
