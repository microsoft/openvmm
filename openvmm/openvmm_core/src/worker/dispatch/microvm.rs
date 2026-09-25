// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Worker construction and lifecycle support for the microVM profile.

#[cfg(guest_arch = "x86_64")]
use openvmm_defs::microvm::MachineProfile;

#[cfg(guest_arch = "x86_64")]
pub(super) fn x86_topology_builder(
    machine_profile: MachineProfile,
) -> anyhow::Result<vm_topology::processor::TopologyBuilder<vm_topology::processor::x86::X86Topology>>
{
    if machine_profile == MachineProfile::Microvm {
        Ok(vm_topology::processor::TopologyBuilder::new_x86())
    } else {
        Ok(vm_topology::processor::TopologyBuilder::from_host_topology()?)
    }
}
