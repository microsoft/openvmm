// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Client definitions for functionality in the `vmm_core` crate.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

pub mod debug_rpc;

use inspect::Inspect;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use std::sync::Arc;

/// Specifies an MMIO range, either by size (the resolver allocates) or by
/// fixed location.
#[derive(Debug, MeshPayload)]
pub enum MmioRangeConfig {
    /// Dynamically allocate a range of the given size.
    Dynamic {
        /// Size of the range in bytes.
        size: u64,
    },
    /// Use the specified fixed memory range.
    Fixed(MemoryRange),
}

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
}
