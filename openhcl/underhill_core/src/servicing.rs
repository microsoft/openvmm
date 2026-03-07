// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Type definitions for the VM's servicing state.

pub use state::*;

use crate::worker::FirmwareType;
use anyhow::Context as _;
use vmcore::save_restore::SavedStateBlob;

mod state {
    use mana_driver::save_restore::ManaSavedState;
    use mesh::payload::Protobuf;
    use openhcl_dma_manager::save_restore::OpenhclDmaManagerState;
    use state_unit::SavedStateUnit;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "underhill")]
    pub struct ServicingState {
        /// State needed for VM initialization.
        #[mesh(1)]
        pub init_state: ServicingInitState,
        /// Saved state from the state units.
        #[mesh(2)]
        pub units: Vec<SavedStateUnit>,
    }

    /// Servicing state needed to optimize emuplat glue on restore.
    #[derive(Protobuf)]
    #[mesh(package = "underhill")]
    pub struct EmuplatSavedState {
        #[mesh(1)]
        pub rtc_local_clock: <crate::emuplat::local_clock::UnderhillLocalClock as SaveRestore>::SavedState,
        #[mesh(2)]
        pub get_backed_adjust_gpa_range: Option<<crate::emuplat::i440bx_host_pci_bridge::GetBackedAdjustGpaRange as SaveRestore>::SavedState>,
        #[mesh(3)]
        pub netvsp_state: Vec<crate::emuplat::netvsp::SavedState>,
    }

    #[derive(Protobuf)]
    #[mesh(package = "underhill")]
    pub struct NvmeSavedState {
        /// NVMe manager (worker) saved state.
        #[mesh(1)]
        pub nvme_state: crate::nvme_manager::save_restore::NvmeManagerSavedState,
    }

    /// Servicing state needed to create the LoadedVm object.
    #[derive(Protobuf)]
    #[mesh(package = "underhill")]
    pub struct ServicingInitState {
        /// The firmware type the VM booted with.
        #[mesh(1)]
        pub firmware_type: Firmware,
        /// The hypervisor reference time when the state units were stopped.
        #[mesh(2)]
        pub vm_stop_reference_time: u64,
        /// The correlation ID to emit with events, activities, etc...
        /// for tracing and diagnostic purposes.
        #[mesh(3)]
        pub correlation_id: Option<guid::Guid>,
        /// Saved state from emuplat glue.
        #[mesh(4)]
        pub emuplat: EmuplatSavedState,
        /// The result of the flush logs request, if there is one.
        #[mesh(5)]
        pub flush_logs_result: Option<FlushLogsResult>,
        /// VMGS related saved state
        #[mesh(6)]
        pub vmgs: Option<(
            vmgs::save_restore::state::SavedVmgsState,
            disk_get_vmgs::save_restore::SavedBlockStorageMetadata,
        )>,
        /// Intercept the host-provided shutdown IC device.
        #[mesh(7)]
        pub overlay_shutdown_device: bool,
        /// NVMe saved state.
        #[mesh(10000)]
        pub nvme_state: Option<NvmeSavedState>,
        /// Dma manager state
        #[mesh(10001)]
        pub dma_manager_state: Option<OpenhclDmaManagerState>,
        #[mesh(10002)]
        pub vmbus_client: Option<vmbus_client::SavedState>,
        #[mesh(10003)]
        pub mana_state: Option<Vec<ManaSavedState>>,
    }

    #[derive(Protobuf)]
    #[mesh(package = "underhill")]
    pub struct FlushLogsResult {
        #[mesh(1)]
        pub duration_us: u64,
        #[mesh(2)]
        pub error: Option<String>,
    }

    /// The VM's firmware type.
    #[derive(Copy, Clone, Protobuf)]
    #[mesh(package = "underhill")]
    pub enum Firmware {
        #[mesh(1)]
        Uefi,
        #[mesh(2)]
        Pcat,
        #[mesh(3)]
        None,
    }
}

impl ServicingState {
    /// Update the state with extra data to ensure it can be restored by older
    /// versions of the paravisor.
    pub fn fix_pre_save(&mut self) -> anyhow::Result<()> {
        // Needed to save to release/2411:
        if let Some(client) = &self.init_state.vmbus_client {
            let vmbus_relay = self.units.iter_mut().find(|x| x.name == "vmbus_relay");
            if let Some(relay_unit) = vmbus_relay {
                let relay = relay_unit
                    .state
                    .parse::<vmbus_relay::SavedState>()
                    .context("failed to parse vmbus relay state")?;

                // Append the legacy saved state to the relay unit state so that
                // older versions of the paravisor can see the old fields and
                // restore from them.
                let legacy_relay =
                    vmbus_relay::legacy_saved_state::SavedState::from_relay_and_client(
                        &relay, client,
                    );
                // TODO: extend the blob instead of replacing it so that both
                // the old and new relay state fields are available, for cross
                // compatibility.
                relay_unit.state = SavedStateBlob::new(legacy_relay);
                // TODO: once the new vmbus client state has stabilized and the
                // TODO above has been addressed, remove this.
                self.init_state.vmbus_client = None;
            }
        }
        Ok(())
    }

    /// Update state that may be coming from older versions of the paravisor to
    /// ensure it can be restored by the current version.
    pub fn fix_post_restore(&mut self) -> anyhow::Result<()> {
        // Needed to restore from release/2411.
        if self.init_state.vmbus_client.is_none() {
            let vmbus_relay = self.units.iter_mut().find(|x| x.name == "vmbus_relay");
            if let Some(relay_unit) = vmbus_relay {
                // Compute the new relay and client saved states from the legacy
                // saved state.
                let mut relay = relay_unit
                    .state
                    .parse::<vmbus_relay::legacy_saved_state::SavedState>()?;
                relay_unit.state = SavedStateBlob::new(relay.relay_saved_state());
                self.init_state.vmbus_client = Some(relay.client_saved_state());
            }
        }
        Ok(())
    }
}

impl From<FirmwareType> for Firmware {
    fn from(value: FirmwareType) -> Self {
        match value {
            FirmwareType::Uefi => Self::Uefi,
            FirmwareType::Pcat => Self::Pcat,
            FirmwareType::None => Self::None,
        }
    }
}

impl From<Firmware> for FirmwareType {
    fn from(value: Firmware) -> Self {
        match value {
            Firmware::Uefi => Self::Uefi,
            Firmware::Pcat => Self::Pcat,
            Firmware::None => Self::None,
        }
    }
}

#[expect(clippy::option_option)]
pub mod transposed {
    use super::*;
    use mana_driver::save_restore::ManaSavedState;
    use openhcl_dma_manager::save_restore::OpenhclDmaManagerState;
    use vmcore::save_restore::SaveRestore;

    /// A transposed `Option<ServicingInitState>`, where each field of
    /// `ServicingInitState` gets wrapped in an `Option`
    #[derive(Default)]
    pub struct OptionServicingInitState {
        pub firmware_type: Option<Firmware>,
        pub vm_stop_reference_time: Option<u64>,
        pub emuplat: OptionEmuplatSavedState,
        pub mana_state: Option<Vec<ManaSavedState>>,
        pub flush_logs_result: Option<Option<FlushLogsResult>>,
        pub vmgs: Option<(
            vmgs::save_restore::state::SavedVmgsState,
            disk_get_vmgs::save_restore::SavedBlockStorageMetadata,
        )>,
        pub overlay_shutdown_device: Option<bool>,
        pub nvme_state: Option<Option<NvmeSavedState>>,
        pub dma_manager_state: Option<Option<OpenhclDmaManagerState>>,
        pub vmbus_client: Option<Option<vmbus_client::SavedState>>,
    }

    /// A transposed `Option<EmuplatSavedState>`, where each field of
    /// `EmuplatSavedState` gets wrapped in an `Option`
    #[derive(Default)]
    pub struct OptionEmuplatSavedState {
        pub rtc_local_clock: Option<<crate::emuplat::local_clock::UnderhillLocalClock as SaveRestore>::SavedState>,
        pub get_backed_adjust_gpa_range: Option<Option<<crate::emuplat::i440bx_host_pci_bridge::GetBackedAdjustGpaRange as SaveRestore>::SavedState>>,
        pub netvsp_state: Option<Vec<crate::emuplat::netvsp::SavedState>>,
    }

    impl From<Option<ServicingInitState>> for OptionServicingInitState {
        fn from(state: Option<ServicingInitState>) -> Self {
            if let Some(state) = state {
                let ServicingInitState {
                    firmware_type,
                    vm_stop_reference_time,
                    correlation_id: _correlation_id,
                    emuplat:
                        EmuplatSavedState {
                            rtc_local_clock,
                            get_backed_adjust_gpa_range,
                            netvsp_state,
                        },
                    flush_logs_result,
                    vmgs,
                    overlay_shutdown_device,
                    nvme_state,
                    mana_state,
                    dma_manager_state,
                    vmbus_client,
                } = state;

                OptionServicingInitState {
                    firmware_type: Some(firmware_type),
                    vm_stop_reference_time: Some(vm_stop_reference_time),
                    emuplat: OptionEmuplatSavedState {
                        rtc_local_clock: Some(rtc_local_clock),
                        get_backed_adjust_gpa_range: Some(get_backed_adjust_gpa_range),
                        netvsp_state: Some(netvsp_state),
                    },
                    flush_logs_result: Some(flush_logs_result),
                    vmgs,
                    overlay_shutdown_device: Some(overlay_shutdown_device),
                    nvme_state: Some(nvme_state),
                    mana_state,
                    dma_manager_state: Some(dma_manager_state),
                    vmbus_client: Some(vmbus_client),
                }
            } else {
                OptionServicingInitState::default()
            }
        }
    }
}
