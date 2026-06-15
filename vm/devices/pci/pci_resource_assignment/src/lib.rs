// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! PCI resource assignment — bus enumeration and BAR address allocation.
//!
//! This crate implements PCI bus enumeration and resource assignment,
//! operating purely through config space reads and writes via the
//! [`PciConfigAccess`] trait.
//!
//! **Phase 1 — Enumerate and probe:** DFS walk assigning bus numbers. At each
//! device, disable MMIO decode and probe BAR sizes by writing all-ones and
//! reading back.
//!
//! **Phase 2 — Assign addresses:** Bottom-up subtree sizing, then top-down
//! address assignment with bridge window programming.

#![forbid(unsafe_code)]

mod assign;
mod enumerate;
mod tests;

/// Compute the devfn byte from a device number and function number.
///
/// This is the standard PCI encoding: `(device << 3) | function`.
pub fn devfn(device: u8, function: u8) -> u8 {
    (device << 3) | function
}

/// Trait abstracting PCI configuration space access.
///
/// The methods are async because PCI config space accesses can be deferred
/// (e.g., for devices that need cross-process communication). The ECAM
/// implementation handles deferred completions; tests use a synchronous mock.
pub trait PciConfigAccess {
    /// Read a 32-bit value from PCI config space.
    fn read_u32(&mut self, bus: u8, devfn: u8, offset: u16) -> impl Future<Output = u32>;

    /// Write a 32-bit value to PCI config space.
    fn write_u32(
        &mut self,
        bus: u8,
        devfn: u8,
        offset: u16,
        value: u32,
    ) -> impl Future<Output = ()>;
}

pub use memory_range::MemoryRange;

/// Parameters for PCI resource assignment on a single host bridge.
#[derive(Debug, Clone)]
pub struct AssignmentParams {
    /// First bus number owned by this host bridge.
    pub start_bus: u8,
    /// Last bus number owned by this host bridge (inclusive).
    pub end_bus: u8,
    /// Low MMIO aperture (below 4 GB) available for BAR allocation.
    /// Empty if no low MMIO is available.
    pub low_mmio: MemoryRange,
    /// High MMIO aperture (above 4 GB) available for 64-bit BAR allocation.
    /// Empty if no high MMIO is available.
    pub high_mmio: MemoryRange,
    /// When true, treat non-zero BAR values found during probing as pinned
    /// addresses rather than clearing them. Used for P2P DMA where GPA = HPA.
    pub preserve_bars: bool,
}

/// Assign PCI resources (bus numbers and BAR addresses) for a host bridge.
///
/// This walks the PCI topology starting at `params.start_bus`, assigns bus
/// numbers to bridges, probes BAR sizes, and programs BAR addresses from
/// the provided MMIO apertures.
pub async fn assign_pci_resources(
    cfg: &mut impl PciConfigAccess,
    params: &AssignmentParams,
) -> Result<(), AssignmentError> {
    let mut devices = enumerate::enumerate_and_probe(cfg, params).await?;
    assign::assign_addresses(&mut devices, params)?;
    assign::program_assignments(cfg, &devices).await;
    Ok(())
}

/// Errors during resource assignment.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AssignmentError {
    /// Ran out of bus numbers during enumeration.
    #[error("bus number exhaustion at bridge {bus:02x}:{device:02x}.{function}")]
    BusExhaustion {
        /// The bridge that needed another bus number.
        bus: u8,
        /// Device number of the bridge.
        device: u8,
        /// Function number of the bridge.
        function: u8,
    },
    /// SR-IOV VF bus numbers conflict with buses already assigned to bridges.
    #[error(
        "SR-IOV VF bus conflict at {bus:02x}:{device:02x}.{function}: \
         VFs need bus {max_vf_bus} but buses up to {next_bus} are already assigned"
    )]
    SriovBusConflict {
        /// Bus of the PF.
        bus: u8,
        /// Device number of the PF.
        device: u8,
        /// Function number of the PF.
        function: u8,
        /// Highest bus number needed by VFs.
        max_vf_bus: u16,
        /// Next bus number already allocated (VF buses below this are taken).
        next_bus: u16,
    },
    /// A pinned BAR address is not naturally aligned to the BAR size.
    #[error(
        "pinned BAR {bus:02x}:{device:02x}.{function} index {bar_index} at {address:#x} \
         is not aligned to {required_alignment:#x}"
    )]
    PinnedBarMisaligned {
        /// Bus number.
        bus: u8,
        /// Device number.
        device: u8,
        /// Function number.
        function: u8,
        /// BAR register index.
        bar_index: u8,
        /// The pinned address.
        address: u64,
        /// Required alignment (BAR size).
        required_alignment: u64,
    },
    /// Two pinned BAR regions overlap.
    #[error(
        "pinned BAR regions overlap: {first_address:#x}..{first_end:#x} and \
         {second_address:#x}..{second_end:#x}"
    )]
    PinnedBarOverlap {
        /// Start of the first pinned region.
        first_address: u64,
        /// End of the first pinned region.
        first_end: u64,
        /// Start of the second pinned region.
        second_address: u64,
        /// End of the second pinned region.
        second_end: u64,
    },
    /// A pinned BAR address falls outside the MMIO aperture.
    #[error(
        "pinned BAR {bus:02x}:{device:02x}.{function} index {bar_index} at \
         {address:#x}+{size:#x} outside {aperture} aperture"
    )]
    PinnedBarOutOfAperture {
        /// Bus number.
        bus: u8,
        /// Device number.
        device: u8,
        /// Function number.
        function: u8,
        /// BAR register index.
        bar_index: u8,
        /// The pinned address.
        address: u64,
        /// BAR size.
        size: u64,
        /// Which aperture was checked.
        aperture: &'static str,
    },
    /// Not enough MMIO space for all BAR allocations.
    #[error("{aperture} MMIO exhaustion: need {required:#x} bytes, have {available:#x}")]
    MmioExhaustion {
        /// Total MMIO required across all devices.
        required: u64,
        /// Total MMIO available.
        available: u64,
        /// Whether this was the low or high aperture.
        aperture: &'static str,
    },
}
