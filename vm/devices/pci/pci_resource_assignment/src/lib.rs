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

/// Trait abstracting PCI configuration space access.
///
/// The methods are async because PCI config space accesses can be deferred
/// (e.g., for devices that need cross-process communication). The ECAM
/// implementation handles deferred completions; tests use a synchronous mock.
pub trait PciConfigAccess {
    /// Read a 32-bit value from PCI config space.
    fn read_u32(
        &mut self,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
    ) -> impl Future<Output = u32>;

    /// Write a 32-bit value to PCI config space.
    fn write_u32(
        &mut self,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) -> impl Future<Output = ()>;
}

/// MMIO aperture available for allocation.
#[derive(Debug, Clone, Copy)]
pub struct MmioAperture {
    /// Base address of the aperture (must be naturally aligned to the
    /// aperture's purpose — typically 1 MB for bridge windows).
    pub base: u64,
    /// Length in bytes.
    pub len: u64,
}

/// Parameters for PCI resource assignment on a single host bridge.
#[derive(Debug, Clone)]
pub struct AssignmentParams {
    /// First bus number owned by this host bridge.
    pub start_bus: u8,
    /// Last bus number owned by this host bridge (inclusive).
    pub end_bus: u8,
    /// Low MMIO aperture (below 4 GB) available for BAR allocation.
    pub low_mmio: Option<MmioAperture>,
    /// High MMIO aperture (above 4 GB) available for 64-bit BAR allocation.
    pub high_mmio: Option<MmioAperture>,
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
    assign_pci_resources_inner(cfg, params).await?;
    Ok(())
}

/// Inner implementation that returns the assignment result for internal use
/// (e.g., tests).
pub(crate) async fn assign_pci_resources_inner(
    cfg: &mut impl PciConfigAccess,
    params: &AssignmentParams,
) -> Result<AssignmentResult, AssignmentError> {
    let devices = enumerate::enumerate_and_probe(cfg, params).await?;
    let assignments = assign::assign_addresses(&devices, params)?;
    assign::program_assignments(cfg, &assignments).await;
    Ok(assignments)
}

/// Result of a successful resource assignment.
#[derive(Debug, Clone)]
struct AssignmentResult {
    /// All device and bridge assignments made.
    entries: Vec<AssignmentEntry>,
}

/// A single device or bridge assignment.
#[derive(Debug, Clone)]
struct AssignmentEntry {
    /// Bus number.
    bus: u8,
    /// Device number.
    device: u8,
    /// Function number.
    function: u8,
    /// BAR assignments (index → address). Only populated BARs are included.
    bars: Vec<BarAssignment>,
    /// For bridges: the secondary bus number assigned.
    secondary_bus: Option<u8>,
    /// For bridges: the subordinate bus number assigned.
    subordinate_bus: Option<u8>,
    /// For bridges: the non-prefetchable memory window base (32-bit only).
    memory_base: Option<u64>,
    /// For bridges: the non-prefetchable memory window limit (32-bit only).
    memory_limit: Option<u64>,
    /// For bridges: the prefetchable memory window base (64-bit capable).
    prefetchable_base: Option<u64>,
    /// For bridges: the prefetchable memory window limit (64-bit capable).
    prefetchable_limit: Option<u64>,
}

/// A BAR address assignment.
#[derive(Debug, Clone)]
struct BarAssignment {
    /// BAR index (0–5 for endpoints, 0–1 for bridges).
    index: u8,
    /// Assigned base address.
    address: u64,
    /// Size in bytes.
    size: u64,
    /// Whether the BAR register is 64-bit (occupies two config space DWORDs).
    is_64bit: bool,
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
