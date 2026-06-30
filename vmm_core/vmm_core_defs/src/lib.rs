// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client definitions for functionality in the `vmm_core` crate.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod debug_rpc;

use inspect::Inspect;
use mesh::MeshPayload;
use mesh::payload::Protobuf;

/// Default memory layout sizing for a VM, used by the layout engine in
/// `openvmm_core::worker::memory_layout`.
///
/// Consumers that receive their memory layout from the host (such as OpenHCL /
/// Underhill) do not use these values.
#[derive(Debug, Clone, MeshPayload)]
pub struct LayoutConfig {
    /// Chipset low MMIO range size (below 4 GiB) for VMOD/PCI0 _CRS.
    /// The address is always allocated dynamically. `0` uses only the
    /// architectural minimum (LAPIC, IOAPIC, GIC, etc.).
    pub chipset_low_mmio_size: u32,
    /// Chipset high MMIO range size (above RAM) for VMOD/PCI0 _CRS.
    /// The address is always allocated dynamically. `0` disables the range.
    pub chipset_high_mmio_size: u64,
    /// VTL2-private chipset MMIO range size for VTL2 VMBus.
    /// The address is always allocated dynamically. `0` disables the range.
    pub vtl2_chipset_mmio_size: u64,
}
use std::sync::Arc;

/// HaltReason sent by devices and vp_set to the vmm.
#[derive(Debug, Clone, Eq, PartialEq, Protobuf, Inspect)]
#[inspect(tag = "halt_reason")]
pub enum HaltReason {
    PowerOff,
    Reset,
    Hibernate,
    DebugBreak {
        #[inspect(rename = "failing_vp")]
        vp: Option<u32>,
    },
    TripleFault {
        #[inspect(rename = "failing_vp")]
        vp: u32,
        #[inspect(skip)]
        // Arc'ed for size and cheap clones.
        registers: Option<Arc<virt::vp::Registers>>,
    },
    SingleStep {
        #[inspect(rename = "failing_vp")]
        vp: u32,
    },
    HwBreakpoint {
        #[inspect(rename = "failing_vp")]
        vp: u32,
        #[inspect(skip)]
        breakpoint: virt::x86::HardwareBreakpoint,
    },
    /// The guest watchdog timer expired without being petted.
    Watchdog,
}
