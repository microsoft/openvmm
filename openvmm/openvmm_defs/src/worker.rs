// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mesh worker definitions for the VM worker.

use crate::config::Config;
use crate::rpc::VmRpc;
use hypervisor_resources::HypervisorKind;
use mesh::MeshPayload;
use mesh::payload::Protobuf;
use mesh::payload::message::ProtobufMessage;
use mesh_worker::WorkerId;
use state_unit::SavedStateUnit;
use state_unit::UnitContract;
use state_unit::UnitLifetime;
use std::collections::BTreeSet;
use vm_resource::Resource;
use vmcore::save_restore::SavedStateRoot;
use vmm_core_defs::HaltReason;

/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(unix)]
pub type SharedMemoryFd = std::os::fd::OwnedFd;
/// File descriptor (Unix) or handle (Windows) for file-backed guest RAM.
#[cfg(windows)]
pub type SharedMemoryFd = std::os::windows::io::OwnedHandle;

pub const VM_WORKER: WorkerId<VmWorkerParameters> = WorkerId::new("VmWorker");

/// Complete saved state consumed by the VM worker.
#[derive(Protobuf, SavedStateRoot)]
#[mesh(package = "openvmm")]
pub struct SavedState {
    #[mesh(1)]
    pub units: Vec<SavedStateUnit>,
    /// The contract of every state unit, including units with no saved state,
    /// ordered by name. Saved state from before OpenVMM recorded contracts has
    /// none.
    #[mesh(2)]
    pub inventory: Vec<UnitContract>,
}

impl SavedState {
    /// Returns the PCIe ports whose devices the inventory records as
    /// hot-plugged, which a restore re-creates.
    pub fn hotplugged_pcie_ports(&self) -> BTreeSet<&str> {
        self.inventory
            .iter()
            .filter(|contract| contract.lifetime == UnitLifetime::Dynamic)
            .filter_map(|contract| vmotherboard::pcie_port_of_stable_id(&contract.stable_id))
            .collect()
    }
}

/// Launch parameters for the VM worker.
#[derive(MeshPayload)]
pub struct VmWorkerParameters {
    /// The hypervisor to use.
    pub hypervisor: Resource<HypervisorKind>,
    /// The initial configuration.
    pub cfg: Config,
    /// The saved state.
    pub saved_state: Option<ProtobufMessage>,
    /// File-backed guest RAM handle. When set, guest memory uses this
    /// fd/handle instead of allocating anonymous memory.
    pub shared_memory: Option<SharedMemoryFd>,
    /// The devices of the PCIe ports that the saved state records as
    /// hot-plugged, which the restore re-creates on those ports, without
    /// notifying the guest, before it applies their saved state.
    pub restore_pcie_devices: Vec<crate::config::PcieDeviceConfig>,
    /// The VM RPC channel.
    pub rpc: mesh::Receiver<VmRpc>,
    /// The notification channel.
    pub notify: mesh::Sender<HaltReason>,
}

#[cfg(test)]
mod tests {
    use super::SavedState;
    use state_unit::UnitContract;
    use state_unit::UnitLifetime;
    use test_with_tracing::test;

    fn contract(name: &str, stable_id: &str, lifetime: UnitLifetime) -> UnitContract {
        UnitContract {
            name: name.to_owned(),
            stable_id: stable_id.to_owned(),
            kind: String::new(),
            lifetime,
            descriptors: Vec::new(),
        }
    }

    #[test]
    fn hotplugged_pcie_ports_are_the_dynamic_pcie_devices() {
        let state = SavedState {
            units: Vec::new(),
            inventory: vec![
                contract("pcie-hotplug:rp1", "pcie-port:rp1", UnitLifetime::Dynamic),
                contract("pcie:rp0-virtio", "pcie-port:rp0", UnitLifetime::Static),
                contract("vpci:1", "vpci:1", UnitLifetime::Dynamic),
                contract("vmtime", "vmtime", UnitLifetime::Derived),
            ],
        };
        assert_eq!(
            state
                .hotplugged_pcie_ports()
                .into_iter()
                .collect::<Vec<_>>(),
            ["rp1"]
        );
    }
}
