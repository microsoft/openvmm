// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod gic_software_device;
pub mod vm;
pub mod vp;

use crate::state::StateElement;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use thiserror::Error;
use vm_topology::processor::aarch64::Aarch64VpInfo;

/// VP state that can be set for initial boot.
#[derive(Debug, PartialEq, Eq, Protobuf)]
pub struct Aarch64InitialRegs {
    /// Register state to be set on the BSP.
    pub registers: vp::Registers,
    /// System register state for the BSP.
    pub system_registers: vp::SystemRegisters,
}

impl Aarch64InitialRegs {
    pub fn at_reset(caps: &Aarch64PartitionCapabilities, bsp: &Aarch64VpInfo) -> Self {
        Self {
            registers: vp::Registers::at_reset(caps, bsp),
            system_registers: vp::SystemRegisters::at_reset(caps, bsp),
        }
    }
}

#[derive(Debug, Inspect)]
pub struct Aarch64PartitionCapabilities {}

#[derive(Error, Debug)]
pub enum Aarch64PartitionCapabilitiesError {}
