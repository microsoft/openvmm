// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Underhill VM worker.

cfg_if::cfg_if! {
    if #[cfg(guest_arch = "x86_64")] {
        pub use hvdef::HvX64RegisterName as HvArchRegisterName;
        use chipset_device_resources::BSP_LINT_LINE_SET;
        use virt::irqcon::MsiRequest;
        use vmm_core::acpi_builder::AcpiTablesBuilder;
    } else if #[cfg(guest_arch = "aarch64")] {
        pub use hvdef::HvArm64RegisterName as HvArchRegisterName;
        use virt::Aarch64Partition;
    }
}

use crate::ControlRequest;
use crate::dispatch::LoadedVm;
use crate::dispatch::LoadedVmNetworkSettings;
use crate::dispatch::vtl2_settings_worker::InitialControllers;
use crate::dispatch::vtl2_settings_worker::disk_from_disk_type;
use crate::dispatch::vtl2_settings_worker::wait_for_mana;
use crate::emuplat::EmuplatServicing;
use crate::emuplat::firmware::UnderhillLogger;
use crate::emuplat::firmware::UnderhillVsmConfig;
use crate::emuplat::framebuffer::FramebufferRemoteControl;
use crate::emuplat::i440bx_host_pci_bridge::ArcMutexGetBackedAdjustGpaRange;
use crate::emuplat::i440bx_host_pci_bridge::GetBackedAdjustGpaRange;
use crate::emuplat::local_clock::ArcMutexUnderhillLocalClock;
use crate::emuplat::local_clock::UnderhillLocalClock;
use crate::emuplat::netvsp::HclNetworkVFManager;
use crate::emuplat::netvsp::HclNetworkVFManagerEndpointInfo;
use crate::emuplat::netvsp::HclNetworkVFManagerShutdownInProgress;
use crate::emuplat::netvsp::RuntimeSavedState;
use crate::emuplat::non_volatile_store::VmgsBrokerNonVolatileStore;
use crate::emuplat::tpm::resources::GetTpmLoggerHandle;
use crate::emuplat::tpm::resources::GetTpmRequestAkCertHelperHandle;
use crate::emuplat::vga_proxy::UhRegisterHostIoFastPath;
use crate::emuplat::watchdog::UnderhillWatchdogPlatform;
use crate::loader::LoadKind;
use crate::loader::vtl0_config::MeasuredVtl0Info;
use crate::loader::vtl2_config::RuntimeParameters;
use crate::nvme_manager::device::VfioNvmeDriverSpawner;
use crate::nvme_manager::manager::NvmeDiskConfig;
use crate::nvme_manager::manager::NvmeDiskResolver;
use crate::nvme_manager::manager::NvmeManager;
use crate::options::GuestStateEncryptionPolicyCli;
use crate::options::GuestStateLifetimeCli;
use crate::options::KeepAliveConfig;
use crate::options::TestScenarioConfig;
use crate::reference_time::ReferenceTime;
use crate::servicing;
use crate::servicing::ServicingState;
use crate::servicing::transposed::OptionServicingInitState;
use crate::threadpool_vm_task_backend::ThreadpoolBackend;
use crate::vmbus_relay_unit::VmbusRelayHandle;
use crate::vmgs_logger::GetVmgsLogger;
use crate::wrapped_partition::WrappedPartition;
use anyhow::Context;
use async_trait::async_trait;
use chipset_device::ChipsetDevice;
use chipset_device_worker_defs::RemoteChipsetDeviceHandle;
use closeable_mutex::CloseableMutex;
use cvm_tracing::CVM_ALLOWED;
use debug_ptr::DebugPtr;
use disk_backend::Disk;
use disk_blockdevice::BlockDeviceResolver;
use disk_blockdevice::OpenBlockDeviceConfig;
use firmware_uefi::LogLevel;
use firmware_uefi::UefiCommandSet;
use futures::executor::block_on;
use futures::future::join_all;
use futures_concurrency::future::Race;
use get_protocol::EventLogId;
use get_protocol::RegisterState;
use get_protocol::TripleFaultType;
use get_protocol::dps_json::GuestStateEncryptionPolicy;
use get_protocol::dps_json::GuestStateLifetime;
use guest_emulation_transport::GuestEmulationTransportClient;
use guest_emulation_transport::api::platform_settings::DevicePlatformSettings;
use guest_emulation_transport::api::platform_settings::General;
use guestmem::GuestMemory;
use guid::Guid;
use hcl_compat_uefi_nvram_storage::HclCompatNvramQuirks;
use hvdef::HvRegisterValue;
use hvdef::Vtl;
use hvdef::hypercall::HvGuestOsId;
use hyperv_ic_guest::ShutdownGuestIc;
use ide_resources::GuestMedia;
use ide_resources::IdePath;
use igvm_defs::MemoryMapEntryType;
use input_core::InputData;
use input_core::MultiplexedInputHandle;
use inspect::Inspect;
use loader_defs::shim::MemoryVtlType;
use mana_driver::save_restore::ManaSavedState;
use memory_range::MemoryRange;
use mesh::CancelContext;
use mesh::MeshPayload;
use mesh::rpc::RpcSend;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use net_packet_capture::PacketCaptureParams;
use openhcl_attestation_protocol::igvm_attest::get::runtime_claims::AttestationVmConfig;
use openhcl_dma_manager::AllocationVisibility;
use openhcl_dma_manager::DmaClientParameters;
use openhcl_dma_manager::DmaClientSpawner;
use openhcl_dma_manager::LowerVtlPermissionPolicy;
use openhcl_dma_manager::OpenhclDmaManager;
use pal_async::DefaultDriver;
use pal_async::DefaultPool;
use pal_async::local::LocalDriver;
use pal_async::task::Spawn;
use parking_lot::Mutex;
use scsi_core::ResolveScsiDeviceHandleParams;
use scsidisk::atapi_scsi::AtapiScsiDisk;
use socket2::Socket;
use state_unit::SpawnedUnit;
use state_unit::StateUnits;
use std::collections::HashMap;
use std::ffi::CString;
use std::future;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use storvsp::ScsiControllerDisk;
use thiserror::Error;
use tpm_resources::TpmAkCertTypeResource;
use tpm_resources::TpmDeviceHandle;
use tpm_resources::TpmRegisterLayout;
use tracing::Instrument;
use tracing::instrument;
use uevent::UeventListener;
use underhill_attestation::AttestationType;
use underhill_confidentiality::confidential_debug_enabled;
use underhill_threadpool::AffinitizedThreadpool;
use underhill_threadpool::ThreadpoolBuilder;
use user_driver::vfio::VfioDmaClients;
use virt::Partition;
use virt::VpIndex;
use virt::X86Partition;
use virt::state::HvRegisterState;
use virt_mshv_vtl::UhPartition;
use virt_mshv_vtl::UhPartitionNewParams;
use virt_mshv_vtl::UhProtoPartition;
use vm_loader::initial_regs::initial_regs;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::KeyboardInputHandleKind;
use vm_resource::kind::MouseInputHandleKind;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::VpInfo;
use vmbus_relay_intercept_device::SimpleVmbusClientDeviceWrapper;
use vmbus_server::VmbusServer;
use vmcore::non_volatile_store::EphemeralNonVolatileStore;
use vmcore::non_volatile_store::resources::EphemeralNonVolatileStoreHandle;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use vmgs::Vmgs;
use vmgs_broker::spawn_vmgs_broker;
use vmgs_resources::VmgsFileHandle;
use vmm_core::input_distributor::InputDistributor;
use vmm_core::partition_unit::Halt;
use vmm_core::partition_unit::PartitionUnit;
use vmm_core::partition_unit::PartitionUnitParams;
use vmm_core::synic::SynicPorts;
use vmm_core::vmbus_unit::ChannelUnit;
use vmm_core::vmbus_unit::VmbusServerHandle;
use vmm_core::vmbus_unit::offer_channel_unit;
use vmm_core::vmbus_unit::offer_vmbus_device_handle_unit;
use vmm_core::vmtime_unit::run_vmtime;
use vmm_core_defs::HaltReason;
use vmotherboard::BaseChipsetBuilder;
use vmotherboard::BaseChipsetBuilderOutput;
use vmotherboard::ChipsetDeviceHandle;
use vmotherboard::options::BaseChipsetDevices;
use vmotherboard::options::BaseChipsetFoundation;
use watchdog_core::platform::WatchdogCallback;
use watchdog_core::platform::WatchdogPlatform;
use zerocopy::FromZeros;

pub(crate) const PM_BASE: u16 = 0x400;
pub(crate) const SYSTEM_IRQ_ACPI: u32 = 9;
pub(crate) const WDAT_PORT: u16 = 0x30;

pub const UNDERHILL_WORKER: WorkerId<UnderhillWorkerParameters> = WorkerId::new("UnderhillWorker");

const MAX_SUBCHANNELS_PER_VNIC: u16 = 32;

// TODO: Move to hsm crate in future.
// AZIHSM VPCI IDs
const AZIHSM_VPCI_VENDOR_ID: u16 = 0x1414;
const AZIHSM_VPCI_DEVICE_ID: u16 = 0xC003;

struct GuestEmulationTransportInfra {
    get_thread: JoinHandle<()>,
    get_spawner: DefaultDriver,
    get_client: GuestEmulationTransportClient,
}

async fn construct_get()
-> Result<(GuestEmulationTransportInfra, pal_async::task::Task<()>), anyhow::Error> {
    // Create a thread to run GET and VMGS clients on.
    //
    // This must be a separate thread from the thread pool because sometimes
    // thread pool threads will block synchronously waiting on the GET or VMGS.
    let (get_thread, get_spawner) = DefaultPool::spawn_on_thread("get");

    let (get_client, get_task) = guest_emulation_transport::spawn_get_worker(get_spawner.clone())
        .await
        .context("failed to launch GET")?;

    // the current policy + implementation of the GET is to treat GET worker
    // failures as fatal, and to tear-down underhill
    let get_watchdog_task = get_spawner.spawn("GET watchdog", async move {
        match get_task.await {
            Ok(()) => {}
            Err(e) => panic!("GET worker has unexpected failed: {:#?}", e),
        }
    });

    Ok((
        GuestEmulationTransportInfra {
            get_thread,
            get_spawner,
            get_client,
        },
        get_watchdog_task,
    ))
}

// Used for locating VM information in a debugger
// Do not use during program execution
static LOADED_VM: DebugPtr<LoadedVm> = DebugPtr::new();

/// The underhill VM worker, used to create and run a VM partition.
pub struct UnderhillVmWorker {
    vm: LoadedVm,
    env_cfg: UnderhillEnvCfg,
    vm_rpc: mesh::Receiver<crate::dispatch::UhVmRpc>,
    is_post_servicing: bool,
    servicing_correlation_id: Option<Guid>,

    get_thread: JoinHandle<()>,
    _get_watchdog_task: Option<pal_async::task::Task<()>>,
    threadpool: AffinitizedThreadpool,
}

/// Underhill configuration specified via env-vars / CLI flags.
#[derive(Debug, MeshPayload, Clone)]
pub struct UnderhillEnvCfg {
    /// Limit the maximum protocol version allowed by vmbus; used for testing purposes.
    pub vmbus_max_version: Option<u32>,
    /// Handle MNF in the Underhill vmbus server, rather than the host.
    pub vmbus_enable_mnf: Option<bool>,
    /// Force the use of confidential external memory for all non-relay vmbus channels.
    pub vmbus_force_confidential_external_memory: bool,
    /// Delay before unsticking a vmbus channel after it has been opened.
    pub vmbus_channel_unstick_delay: Option<Duration>,
    /// Command line to append to VTL0 command line. Only used for linux direct.
    pub cmdline_append: Option<String>,
    /// (dev feature) Reformat VMGS file on boot
    pub reformat_vmgs: bool,
    /// (dev feature) Start the VM with VTL0 paused
    pub vtl0_starts_paused: bool,
    /// If true, emulated serial should not poll data until the guest sets DTR
    /// and RTS.
    pub emulated_serial_wait_for_rts: bool,
    /// Force load the specified image in VTL0. The image must support the
    /// option specified.
    ///
    /// Valid options are "pcat, uefi, linux".
    pub force_load_vtl0_image: Option<String>,
    /// Use the user-mode NVMe driver.
    pub nvme_vfio: bool,
    // TODO MCR: support closed-source configuration logic for MCR device
    pub mcr: bool,
    /// Halt on a guest halt request instead of forwarding to the host.
    pub halt_on_guest_halt: bool,
    /// Leave sidecar VPs remote even if they hit exits.
    pub no_sidecar_hotplug: bool,
    /// Enables the GDB stub for debugging the guest.
    pub gdbstub: bool,
    /// Hide the isolation mode from the guest.
    pub hide_isolation: bool,
    /// Enable nvme keep alive.
    pub nvme_keep_alive: KeepAliveConfig,
    /// Enable mana keep alive.
    pub mana_keep_alive: KeepAliveConfig,
    /// Don't skip FLR for NVMe devices.
    pub nvme_always_flr: bool,
    /// test configuration
    pub test_configuration: Option<TestScenarioConfig>,
    /// Disable the UEFI front page.
    pub disable_uefi_frontpage: Option<bool>,
    /// Always attempt a default boot
    pub default_boot_always_attempt: Option<bool>,
    /// Guest state lifetime
    pub guest_state_lifetime: Option<GuestStateLifetimeCli>,
    /// Guest state encryption policy
    pub guest_state_encryption_policy: Option<GuestStateEncryptionPolicyCli>,
    /// Strict guest state encryption policy
    pub strict_encryption_policy: Option<bool>,
    /// Attempt to renew the AK cert
    pub attempt_ak_cert_callback: Option<bool>,
    /// Enable the VPCI relay
    pub enable_vpci_relay: Option<bool>,
    /// Disable proxy interrupt redirection
    pub disable_proxy_redirect: bool,
    /// Disable lower VTL timer virtualization
    pub disable_lower_vtl_timer_virt: bool,
    /// The timeout in seconds for VM config operations, both the initial configuration
    /// and then subsequent modifications.
    pub config_timeout_in_seconds: u64,
}

/// Bundle of config + runtime objects for hooking into the underhill remote
/// console.
#[derive(Debug, MeshPayload)]
pub struct UnderhillRemoteConsoleCfg {
    pub synth_keyboard: bool,
    pub synth_mouse: bool,
    pub synth_video: bool,
    pub input: mesh::Receiver<InputData>,
    pub framebuffer: Option<framebuffer::Framebuffer>,
}

#[derive(Debug, MeshPayload)]
pub struct UnderhillWorkerParameters {
    pub env_cfg: UnderhillEnvCfg,
    pub remote_console_cfg: UnderhillRemoteConsoleCfg,
    pub debugger_rpc: Option<mesh::Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
    pub vm_rpc: mesh::Receiver<crate::dispatch::UhVmRpc>,
    pub control_send: mesh::Sender<ControlRequest>,
}

#[derive(MeshPayload)]
pub struct RestartState {
    params: UnderhillWorkerParameters,
    servicing_state: ServicingState,
}

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FirmwareType {
    Uefi,
    Pcat,
    None,
}

#[derive(Debug, MeshPayload)]
pub struct NicConfig {
    pub pci_id: String,
    pub instance_id: Guid,
    pub subordinate_instance_id: Option<Guid>,
    pub max_sub_channels: Option<u16>,
}

impl Worker for UnderhillVmWorker {
    type Parameters = UnderhillWorkerParameters;
    type State = RestartState;

    const ID: WorkerId<Self::Parameters> = UNDERHILL_WORKER;

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        pal_async::local::block_with_io(async |driver| {
            let (get_infra, get_watchdog_task) = construct_get().await?;
            let get_client = get_infra.get_client.clone();

            let result = Self::new_or_restart(get_infra, params, true, None, driver).await;

            if let Err(err) = &result {
                tracing::error!(
                    CVM_ALLOWED,
                    error = err.as_ref() as &dyn std::error::Error,
                    "failed to start VM"
                );

                // Note that this probably will not return, since the host
                // should terminate the VM in this case.
                // Format error as raw string because the error is anyhow::Error
                get_client
                    .complete_start_vtl0(Some(format!("{:#}", err)))
                    .await;
            } else {
                get_client.complete_start_vtl0(None).await;
            }

            result.map(|worker| UnderhillVmWorker {
                _get_watchdog_task: Some(get_watchdog_task),
                ..worker
            })
        })
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        pal_async::local::block_with_io(async |driver| {
            let (get_infra, get_watchdog_task) = construct_get().await?;
            let result = Self::new_or_restart(
                get_infra,
                state.params,
                false,
                Some(state.servicing_state),
                driver,
            )
            .await;

            result.map(|worker| UnderhillVmWorker {
                _get_watchdog_task: Some(get_watchdog_task),
                ..worker
            })
        })
    }

    fn run(self, worker_rpc: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        let state = block_on(self.vm.run(
            &self.threadpool,
            self.is_post_servicing || !self.env_cfg.vtl0_starts_paused,
            self.servicing_correlation_id,
            self.vm_rpc,
            worker_rpc,
        ));
        tracing::info!(CVM_ALLOWED, "terminating worker");
        self.get_thread.join().unwrap();
        if let Some(state) = state {
            let params = UnderhillWorkerParameters {
                env_cfg: self.env_cfg,
                remote_console_cfg: UnderhillRemoteConsoleCfg {
                    synth_keyboard: false,
                    synth_mouse: false,
                    synth_video: false,
                    input: mesh::Receiver::new(),
                    framebuffer: None,
                },
                debugger_rpc: None,
                vm_rpc: state.vm_rpc,
                control_send: state.control_send,
            };

            tracing::info!(CVM_ALLOWED, "sending worker restart state");
            state.restart_rpc.complete(Ok(RestartState {
                params,
                servicing_state: state.servicing_state,
            }))
        }
        Ok(())
    }
}

impl UnderhillVmWorker {
    #[instrument(name = "init", skip_all, fields(CVM_ALLOWED))]
    async fn new_or_restart(
        get_infra: GuestEmulationTransportInfra,
        params: UnderhillWorkerParameters,
        boot_init: bool,
        mut servicing_state: Option<ServicingState>,
        early_init_driver: LocalDriver,
    ) -> anyhow::Result<Self> {
        let GuestEmulationTransportInfra {
            get_thread,
            get_spawner,
            get_client,
        } = get_infra;

        // TODO: consider saving/restoring these across servicing instead of
        // re-fetching them (but then how would we know we need to get the
        // servicing state from the host? Classic catch-22.)
        let dps = read_device_platform_settings(&get_client)
            .instrument(tracing::info_span!("init/dps", CVM_ALLOWED))
            .await?;

        // Build the thread pool now that we know the IO ring size to use.
        let threadpool = {
            let io_ring_size = {
                if let Some(vtl2_settings) = dps.general.vtl2_settings.as_ref() {
                    vtl2_settings.fixed.io_ring_size
                } else {
                    // reasonable default
                    256
                }
            };

            // Restrict the number of bounded workers to avoid allocating too many
            // threads waiting on storage tickets from the blk layer.
            //
            // Note that this limit is per NUMA node, so lots of cross-node IO could
            // result in exceeding this limit.
            ThreadpoolBuilder::new()
                .max_bounded_workers(64)
                .ring_size(io_ring_size)
                .build()
                .context("failed to create thread pool")?
        };

        // In a servicing scenario where the saved state is held on the VM host,
        // we only know that saved state exists after we get the DPS information.
        let saved_state_from_host = dps.general.is_servicing_scenario;

        if saved_state_from_host {
            assert!(
                servicing_state.is_none(),
                "cannot have saved state from two different sources"
            );

            if let Some(TestScenarioConfig::RestoreStuck) = params.env_cfg.test_configuration {
                tracing::info!(
                    "Test configuration SERVICING_RESTORE_STUCK is set. Waiting indefinitely in restore."
                );
                future::pending::<()>().await;
            }

            tracing::info!(
                CVM_ALLOWED,
                "VTL2 restart, getting servicing state from the host"
            );

            let saved_state_buf = get_client
                .get_saved_state_from_host()
                .instrument(tracing::info_span!("init/get_saved_state", CVM_ALLOWED))
                .await
                .context("Failed to get saved state from host")?;

            servicing_state = Some(
                mesh::payload::decode(&saved_state_buf)
                    .context("failed to decode servicing state")?,
            );

            tracing::info!(
                CVM_ALLOWED,
                saved_state_len = saved_state_buf.len(),
                "received servicing state from host"
            );
        }

        if let Some(state) = &mut servicing_state {
            state
                .fix_post_restore()
                .context("failed to fix up servicing state on restore")?;
        }

        let is_post_servicing = servicing_state.is_some();
        let correlation_id = (servicing_state.as_ref()).and_then(|s| s.init_state.correlation_id);
        let (servicing_init_state, servicing_unit_state) = match servicing_state {
            None => (None, None),
            Some(ServicingState { init_state, units }) => (Some(init_state), Some(units)),
        };

        // Build the VM.
        let mut vm = new_underhill_vm(
            get_spawner,
            &threadpool,
            early_init_driver,
            UhVmParams {
                get_client: get_client.clone(),
                dps,
                servicing_state: servicing_init_state,
                boot_init,
                env_cfg: params.env_cfg.clone(),
                remote_console_cfg: params.remote_console_cfg,
                debugger_rpc: params.debugger_rpc,
                control_send: params.control_send,
            },
        )
        .instrument(tracing::info_span!(
            "init/new_underhill_vm",
            CVM_ALLOWED,
            correlation_id = correlation_id.map(tracing::field::display)
        ))
        .await?;

        LOADED_VM.store(&vm);

        // Restore state units
        if let Some(unit_state) = servicing_unit_state {
            let r = vm
                .restore_units(unit_state)
                .instrument(tracing::info_span!(
                    "init/restore",
                    CVM_ALLOWED,
                    correlation_id = correlation_id.map(tracing::field::display)
                ))
                .await;

            // If we received saved state from the host then notify the host
            // that servicing was successful.
            //
            // TODO: send error string to host.
            if saved_state_from_host {
                get_client.report_restore_result_to_host(r.is_ok()).await;
            }

            r.context("failed to restore")?;
        }

        Ok(Self {
            vm,
            env_cfg: params.env_cfg,
            vm_rpc: params.vm_rpc,
            get_thread,
            _get_watchdog_task: None,
            threadpool,
            is_post_servicing,
            servicing_correlation_id: correlation_id,
        })
    }
}

struct UhVmParams {
    /// GET client.
    get_client: GuestEmulationTransportClient,
    /// The validated device platform settings sent from the host.
    dps: DevicePlatformSettings,
    /// Provides non-recoverable configuration needed for servicing.
    servicing_state: Option<servicing::ServicingInitState>,
    /// Perform boot initialization tasks such as protecting VTL2 memory.
    boot_init: bool,
    /// Environment configuration.
    env_cfg: UnderhillEnvCfg,
    /// Remote console configuration for VNC support.
    remote_console_cfg: UnderhillRemoteConsoleCfg,
    /// VTL0 debugger requests.
    debugger_rpc: Option<mesh::Receiver<vmm_core_defs::debug_rpc::DebugRequest>>,
    /// Channel to send a prepare for shutdown request to the control process.
    /// This is used before sending a message to the host that will result in an
    /// unclean termination of the VM (such as guest-requested power state
    /// transitions, triple faults, and host-driven VTL2 servicing).
    control_send: mesh::Sender<ControlRequest>,
}

async fn read_device_platform_settings(
    get_client: &GuestEmulationTransportClient,
) -> anyhow::Result<DevicePlatformSettings> {
    let dps = get_client
        .device_platform_settings()
        .await
        .context("failed to get device platform settings")?;

    // TODO: figure out if we really need to trace this. These are too long for
    // the Underhill trace buffer.
    tracing::debug!("device platform settings {:?}", dps);

    Ok(dps)
}

#[derive(Error, Debug)]
pub enum NetworkSettingsError {
    #[error("VF Manager already exists for interface id: {0}")]
    VFManagerExists(Guid),
    #[error("VF Manager does not exist for interface id: {0}")]
    VFManagerMissing(Guid),
    #[error("Network Settings missing")]
    NetworkSettingsMissing,
    #[error("RuntimeSavedState missing for interface id: {0}")]
    RuntimeSavedStateMissing(Guid),
}

#[derive(Inspect)]
struct UhVmNetworkSettings {
    #[inspect(skip)]
    nics: Vec<(Guid, SpawnedUnit<ChannelUnit<netvsp::Nic>>)>,
    #[inspect(iter_by_key)]
    vf_managers: HashMap<Guid, Arc<HclNetworkVFManager>>,
    #[inspect(skip)]
    get_client: GuestEmulationTransportClient,
    #[inspect(skip)]
    vp_count: usize,
    #[inspect(skip)]
    dma_mode: net_mana::GuestDmaMode,
}

impl UhVmNetworkSettings {
    /// Encapsulates the common logic for beginning VF shutdown / save:
    fn begin_vf_teardown(
        &mut self,
        vf_managers: &mut Vec<(Guid, Arc<HclNetworkVFManager>)>,
        remove_vtl0_vf: bool,
    ) -> (
        Vec<(Guid, HclNetworkVFManagerShutdownInProgress)>,
        Vec<(Guid, SpawnedUnit<ChannelUnit<netvsp::Nic>>)>,
    ) {
        // Notify VF managers of shutdown so that the subsequent teardown of
        // the NICs does not modify VF state.
        let vf_managers = vf_managers
            .drain(..)
            .map(move |(instance_id, manager)| {
                (
                    instance_id,
                    Arc::into_inner(manager)
                        .unwrap()
                        .shutdown_begin(remove_vtl0_vf),
                )
            })
            .collect::<Vec<(Guid, HclNetworkVFManagerShutdownInProgress)>>();

        // Collect the instance_id of every vf_manager being shutdown
        let instance_ids: Vec<Guid> = vf_managers
            .iter()
            .map(|(instance_id, _)| *instance_id)
            .collect();

        // Only remove the vmbus channels and NICs from the VF Managers
        let mut nic_channels = Vec::new();
        let mut i = 0;
        while i < self.nics.len() {
            if instance_ids.contains(&self.nics[i].0) {
                let val = self.nics.remove(i);
                nic_channels.push(val);
            } else {
                i += 1;
            }
        }

        for instance_id in instance_ids {
            if !nic_channels.iter().any(|(id, _)| *id == instance_id) {
                tracing::error!(CVM_ALLOWED,
                    %instance_id,
                    "No vmbus channel found that matches VF Manager instance_id"
                );
            }
        }

        (vf_managers, nic_channels)
    }

    async fn shutdown_vf_devices(
        &mut self,
        vf_managers: &mut Vec<(Guid, Arc<HclNetworkVFManager>)>,
        remove_vtl0_vf: bool,
        keep_vf_alive: bool,
    ) {
        let (mut vf_managers, mut nic_channels) =
            self.begin_vf_teardown(vf_managers, remove_vtl0_vf);

        // Close vmbus channels and drop all of the NICs.
        let mut endpoints: Vec<_> =
            join_all(nic_channels.drain(..).map(async |(instance_id, channel)| {
                async {
                    let nic = channel.remove().await.revoke().await;
                    nic.shutdown()
                }
                .instrument(tracing::info_span!("nic_shutdown", CVM_ALLOWED, %instance_id))
                .await
            }))
            .await;

        let shutdown_vfs = join_all(vf_managers.drain(..).map(
            async |(instance_id, mut manager)| {
                manager
                    .complete(keep_vf_alive)
                    .instrument(
                        tracing::info_span!("vf_manager_shutdown", CVM_ALLOWED, %instance_id),
                    )
                    .await
            },
        ));
        let run_endpoints = async {
            loop {
                let _ = endpoints
                    .iter_mut()
                    .map(|endpoint| endpoint.wait_for_endpoint_action())
                    .collect::<Vec<_>>()
                    .race()
                    .await;
            }
        };
        // Complete shutdown on the VFs. Process events on the endpoints to
        // allow for proper shutdown.
        // run_endpoints is a loop, so the race completes when shutdown_vfs completes.
        // Also, run_endpoints races wait_for_endpoint_action() so it doesn't guarentee
        // all endpoints will close their channels before shutdown_vfs completes.
        let _ = (shutdown_vfs, run_endpoints).race().await;
    }

    async fn new_underhill_nic(
        &mut self,
        nic_config: NicConfig,
        vps_count: usize,
        get_client: GuestEmulationTransportClient,
        driver_source: &VmTaskDriverSource,
        uevent_listener: &UeventListener,
        servicing_netvsp_state: &Option<Vec<crate::emuplat::netvsp::SavedState>>,
        partition: Arc<UhPartition>,
        state_units: &StateUnits,
        tp: &AffinitizedThreadpool,
        vmbus_server: &Option<VmbusServerHandle>,
        dma_client_spawner: DmaClientSpawner,
        is_isolated: bool,
        keepalive_mode: KeepAliveConfig,
        saved_mana_state: Option<&ManaSavedState>,
    ) -> anyhow::Result<RuntimeSavedState> {
        let instance_id = nic_config.instance_id;
        let nic_max_sub_channels = nic_config
            .max_sub_channels
            .unwrap_or(MAX_SUBCHANNELS_PER_VNIC)
            .min(vps_count as u16);

        let allocation_visibility = if is_isolated {
            AllocationVisibility::Shared
        } else {
            AllocationVisibility::Private
        };

        let ephemeral_dma_client = dma_client_spawner.new_client(DmaClientParameters {
            device_name: format!("nic_{}_ephemeral", nic_config.pci_id),
            lower_vtl_policy: LowerVtlPermissionPolicy::Any,
            allocation_visibility,
            persistent_allocations: false,
        })?;

        // We need a persistent client if keepalive is enabled or if there is a
        // private pool present without keepalive that needs to free previously
        // persisted memory ranges
        let persistent_dma_client = if keepalive_mode.is_enabled() || saved_mana_state.is_some() {
            Some(dma_client_spawner.new_client(DmaClientParameters {
                device_name: format!("nic_{}", nic_config.pci_id),
                lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                persistent_allocations: true,
                allocation_visibility,
            })?)
        } else {
            None
        };

        let dma_clients = if let Some(persistent_dma_client) = persistent_dma_client {
            VfioDmaClients::Split {
                ephemeral: ephemeral_dma_client,
                persistent: persistent_dma_client,
            }
        } else {
            VfioDmaClients::EphemeralOnly(ephemeral_dma_client)
        };

        let (vf_manager, endpoints, save_state) = HclNetworkVFManager::new(
            nic_config.instance_id,
            nic_config.pci_id,
            nic_config.subordinate_instance_id,
            get_client,
            driver_source,
            uevent_listener,
            vps_count as u32,
            nic_max_sub_channels,
            servicing_netvsp_state,
            self.dma_mode,
            keepalive_mode,
            dma_clients,
            saved_mana_state,
        )
        .await?;

        let ready_ports = Arc::new(futures::lock::Mutex::new(
            (0..endpoints.len()).map(|_| false).collect::<Vec<bool>>(),
        ));
        let vf_manager = Arc::new(vf_manager);
        for (
            i,
            HclNetworkVFManagerEndpointInfo {
                adapter_index,
                mac_address,
                endpoint,
            },
        ) in endpoints.into_iter().enumerate()
        {
            let vmbus_instance_id = {
                let m = mac_address.to_bytes();
                // Some guest behaviors requires the nic interfaces to be enumerated in a
                // particular order. vmbus channel offers are by default sorted using the
                // instance id. Leverage that to sort the network offers based on the
                // vport index.
                Guid {
                    data1: 0xf8615163, // keeping it same as netvsp `interface_id:data1` for ease of search.
                    data2: i as u16,
                    data3: 1 << 12, // type 1 GUID
                    data4: [0x20, 0, m[0], m[1], m[2], m[3], m[4], m[5]], // variant 2
                }
            };
            let p = partition.clone();
            let get_guest_os_id = move || -> HvGuestOsId {
                p.vtl0_guest_os_id()
                    .expect("cannot fail to query the guest OS ID")
            };

            let mut nic_builder = netvsp::Nic::builder()
                .limit_ring_buffer(true)
                .get_guest_os_id(Box::new(get_guest_os_id))
                .max_queues(nic_max_sub_channels);
            let ready_ports = ready_ports.clone();
            nic_builder = nic_builder.virtual_function(
                vf_manager
                    .clone()
                    .create_function(move |cur_state| {
                        let ready_ports = ready_ports.clone();
                        Box::pin(async move {
                            let mut locked_ready = ready_ports.lock().await;
                            locked_ready[i] = true;
                            // If all ports are ready, offer the VTL0 VF device. Once the device is offered, leave
                            // it in the guest until it is completely unused.
                            if locked_ready.iter().all(|cur| *cur != cur_state) {
                                !cur_state
                            } else {
                                cur_state
                            }
                        })
                    })
                    .await?,
            );
            let nic = nic_builder.build(
                driver_source,
                vmbus_instance_id,
                endpoint,
                mac_address,
                adapter_index,
            );

            let channel = offer_channel_unit(
                tp,
                state_units,
                vmbus_server
                    .as_ref()
                    .context("networking requires vmbus redirection to be configured")?,
                nic,
            )
            .await
            .context("failed to offer netvsp channel")?;

            self.nics.push((instance_id, channel));
        }

        self.vf_managers.insert(instance_id, vf_manager);
        Ok(save_state)
    }

    async fn accelerated_device_visible_to_guest(&self, visible: bool) -> anyhow::Result<()> {
        join_all(
            self.vf_managers
                .values()
                .map(|vf_manager| vf_manager.hide_vtl0_instance(!visible)),
        )
        .await
        .into_iter()
        .collect::<anyhow::Result<()>>()
    }
}

#[async_trait]
impl LoadedVmNetworkSettings for UhVmNetworkSettings {
    async fn modify_network_settings(
        &mut self,
        instance_id: Guid,
        subordinate_instance_id: Option<Guid>,
    ) -> anyhow::Result<()> {
        let vf_manager = self
            .vf_managers
            .get(&instance_id)
            .context(format!("unrecognized accelerated network ID {instance_id}"))?;
        vf_manager
            .update_vtl0_instance_id(subordinate_instance_id, self.get_client.clone())
            .await
    }

    async fn add_network(
        &mut self,
        instance_id: Guid,
        subordinate_instance_id: Option<Guid>,
        max_sub_channels: Option<u16>,
        threadpool: &AffinitizedThreadpool,
        uevent_listener: &UeventListener,
        servicing_netvsp_state: &Option<Vec<crate::emuplat::netvsp::SavedState>>,
        partition: Arc<UhPartition>,
        state_units: &StateUnits,
        vmbus_server: &Option<VmbusServerHandle>,
        dma_client_spawner: DmaClientSpawner,
        is_isolated: bool,
        save_restore_supported: KeepAliveConfig,
        mana_state: Option<&ManaSavedState>,
    ) -> anyhow::Result<RuntimeSavedState> {
        if self.vf_managers.contains_key(&instance_id) {
            return Err(NetworkSettingsError::VFManagerExists(instance_id).into());
        }

        let mut ctx = CancelContext::new().with_timeout(Duration::from_secs(5));
        let pci_id = ctx
            .until_cancelled(wait_for_mana(uevent_listener, &instance_id))
            .await
            .context("cancelled waiting for mana devices")??;

        let nic_config = NicConfig {
            pci_id,
            instance_id,
            subordinate_instance_id,
            max_sub_channels,
        };

        let driver_source = VmTaskDriverSource::new(ThreadpoolBackend::new(threadpool.clone()));
        let save_state = self
            .new_underhill_nic(
                nic_config,
                self.vp_count,
                self.get_client.clone(),
                &driver_source,
                uevent_listener,
                servicing_netvsp_state,
                partition,
                state_units,
                threadpool,
                vmbus_server,
                dma_client_spawner,
                is_isolated,
                save_restore_supported,
                mana_state,
            )
            .await?;

        Ok(save_state)
    }

    async fn remove_network(&mut self, instance_id: Guid) -> anyhow::Result<()> {
        let vf_manager = self
            .vf_managers
            .remove_entry(&instance_id)
            .ok_or(NetworkSettingsError::VFManagerMissing(instance_id));

        self.shutdown_vf_devices(&mut vec![vf_manager.unwrap()], true, false)
            .await;
        Ok(())
    }

    async fn unload_for_servicing(&mut self) {
        let mut vf_managers: Vec<(Guid, Arc<HclNetworkVFManager>)> =
            self.vf_managers.drain().collect();
        self.shutdown_vf_devices(&mut vf_managers, false, true)
            .await;
    }

    async fn prepare_for_hibernate(&self) {
        // Remove any accelerated devices before notifying the guest to hibernate.
        if let Err(err) = self.accelerated_device_visible_to_guest(false).await {
            tracing::error!(
                CVM_ALLOWED,
                error = err.as_ref() as &dyn std::error::Error,
                "Failed preparing accelerated network devices for hibernate"
            );
        }
    }

    async fn cancel_prepare_for_hibernate(&self) {
        // Add back any accelerated devices hidden in preparation for hibernate.
        if let Err(err) = self.accelerated_device_visible_to_guest(true).await {
            tracing::error!(
                CVM_ALLOWED,
                error = err.as_ref() as &dyn std::error::Error,
                "Failed restoring accelerated network devices for cancelled hibernate"
            );
        }
    }

    async fn packet_capture(
        &self,
        mut params: PacketCaptureParams<Socket>,
    ) -> anyhow::Result<PacketCaptureParams<Socket>> {
        for manager in self.vf_managers.values() {
            params = manager.packet_capture(params).await?;
        }
        Ok(params)
    }

    async fn save(&mut self) -> Option<Vec<ManaSavedState>> {
        let mut vf_managers: Vec<(Guid, Arc<HclNetworkVFManager>)> =
            self.vf_managers.drain().collect();

        // Nothing to save
        if vf_managers.is_empty() {
            return None;
        }

        let (vf_managers, mut nic_channels) = self.begin_vf_teardown(&mut vf_managers, false);

        let mut endpoints: Vec<_> =
            join_all(nic_channels.drain(..).map(async |(instance_id, channel)| {
                async {
                    let nic = channel.remove().await.revoke().await;
                    nic.shutdown()
                }
                .instrument(tracing::info_span!("nic_shutdown", %instance_id))
                .await
            }))
            .await;

        let run_endpoints = async {
            loop {
                let _ = endpoints
                    .iter_mut()
                    .map(|endpoint| endpoint.wait_for_endpoint_action())
                    .collect::<Vec<_>>()
                    .race()
                    .await;
            }
        };

        let save_vf_managers = join_all(
            vf_managers
                .into_iter()
                .map(|(_, vf_manager)| vf_manager.save()),
        );

        let state = (run_endpoints, save_vf_managers).race().await;

        // Discard any vf_managers that failed to return valid save state.
        Some(state.into_iter().flatten().collect())
    }
}

/// The final vtl0 memory layout computed from different inputs.
struct BuiltVtl0MemoryLayout {
    /// The vtl0 memory map including igvm types.
    vtl0_memory_map: Vec<(MemoryRangeWithNode, MemoryMapEntryType)>,
    /// The vtl0 memory layout.
    vtl0_memory_layout: MemoryLayout,
    /// The memory reserved for shared visibility pool allocations by VTL2.
    shared_pool: Vec<MemoryRangeWithNode>,
    /// The complete memory layout which includes vtl0 memory and the shared
    /// pool. This should never be reported to vtl0, but is used to build the
    /// correct virtstack views of memory for devices for DMA.
    complete_memory_layout: MemoryLayout,
}

/// Build the VTL0 memory map after carving out any memory requested for shared
/// visibility memory to be used by VTL2.
fn build_vtl0_memory_layout(
    vtl0_memory_map: Vec<(MemoryRangeWithNode, MemoryMapEntryType)>,
    mmio: &[MemoryRange],
    mut shared_pool_size: u64,
) -> anyhow::Result<BuiltVtl0MemoryLayout> {
    // Allocate shared_pool memory starting from the last (top of memory)
    // continuing downward until the size is covered.
    //
    // Note that we must only allocate from memory entries that are actually
    // ram, not reserved or pmem. Keep track of the ranges that are skipped to
    // re-add to the memory map later.
    let mut filtered_vtl0_memory_map = vtl0_memory_map.clone();
    let mut skipped = Vec::new();
    let mut shared_pool = Vec::new();
    while shared_pool_size != 0 {
        let (last, last_typ) = filtered_vtl0_memory_map.last_mut().with_context(|| {
            format!(
                "unable to allocate shared_pool of size {shared_pool_size} from VTL0 memory map"
            )
        })?;

        if *last_typ != MemoryMapEntryType::MEMORY {
            skipped.push(filtered_vtl0_memory_map.pop().expect("is element"));
            continue;
        }

        if last.range.len() > shared_pool_size {
            // Split this memory range, with the top being given to the shared
            // pool and the remainder back to VTL0.
            let shared_start = last.range.end() - shared_pool_size;
            let vtl0 = MemoryRangeWithNode {
                range: MemoryRange::new(last.range.start()..shared_start),
                vnode: last.vnode,
            };

            shared_pool.push(MemoryRangeWithNode {
                range: MemoryRange::new(shared_start..last.range.end()),
                vnode: last.vnode,
            });
            *last = vtl0;
            break;
        } else {
            shared_pool_size -= last.range.len();
            shared_pool.push(last.clone());
            filtered_vtl0_memory_map.pop();
        }
    }

    // Skipped entries are added in reverse order to maintain the sorted list.
    for entry in skipped.into_iter().rev() {
        filtered_vtl0_memory_map.push(entry);
    }

    // TODO: SGX ranges get reported as memory here. Correct or not? Probably should remove?
    let memory = filtered_vtl0_memory_map
        .iter()
        .map(|(entry, _typ)| entry.clone())
        .collect::<Vec<_>>();

    let vtl0_memory_layout =
        MemoryLayout::new_from_ranges(&memory, mmio).context("invalid memory layout")?;

    let complete_memory = vtl0_memory_map
        .iter()
        .map(|(entry, _typ)| entry.clone())
        .collect::<Vec<_>>();
    let complete_memory_layout = MemoryLayout::new_from_ranges(&complete_memory, mmio)
        .context("invalid complete memory layout")?;

    tracing::info!(
        CVM_ALLOWED,
        vtl0_ram = vtl0_memory_layout
            .ram()
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<String>>()
            .join(", "),
        "vtl0 ram"
    );

    tracing::info!(
        CVM_ALLOWED,
        vtl0_mmio = vtl0_memory_layout
            .mmio()
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<String>>()
            .join(", "),
        "vtl0 mmio"
    );

    tracing::info!(
        CVM_ALLOWED,
        shared_pool = shared_pool
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<String>>()
            .join(", "),
        "shared pool",
    );

    Ok(BuiltVtl0MemoryLayout {
        vtl0_memory_map: filtered_vtl0_memory_map,
        vtl0_memory_layout,
        shared_pool,
        complete_memory_layout,
    })
}

fn round_up_to_2mb(bytes: u64) -> u64 {
    (bytes + (2 * 1024 * 1024) - 1) & !((2 * 1024 * 1024) - 1)
}

#[cfg(guest_arch = "x86_64")]
fn new_x86_topology(
    cpus: &[bootloader_fdt_parser::Cpu],
    x2apic: vm_topology::processor::x86::X2ApicState,
) -> anyhow::Result<ProcessorTopology<vm_topology::processor::x86::X86Topology>> {
    let vps = {
        let mut vps = cpus
            .iter()
            .enumerate()
            .map(|(vp_index, cpu)| vm_topology::processor::x86::X86VpInfo {
                base: VpInfo {
                    vp_index: VpIndex::new(vp_index as u32),
                    vnode: cpu.vnode,
                },
                apic_id: cpu.reg as u32,
            })
            .collect::<Vec<_>>();
        vps.sort_by_key(|vp| vp.base.vp_index);
        vps
    };

    // Query the processor topology from the CPUID leaves.
    TopologyBuilder::from_host_topology()
        .context("failed to build topology from host")?
        .x2apic(x2apic)
        .build_with_vp_info(vps)
        .context("failed to construct the processor topology")
}

#[cfg(guest_arch = "aarch64")]
fn new_aarch64_topology(
    gic: vm_topology::processor::aarch64::GicInfo,
    cpus: &[bootloader_fdt_parser::Cpu],
    pmu_gsiv: u32,
) -> anyhow::Result<ProcessorTopology<vm_topology::processor::aarch64::Aarch64Topology>> {
    // TODO SMP: Query the MT property from the host topology somehow. Device Tree
    // doesn't specify that.
    let gic_redistributors_base = gic.gic_redistributors_base;
    TopologyBuilder::new_aarch64(gic, pmu_gsiv)
        .vps_per_socket(cpus.len() as u32)
        .build_with_vp_info(cpus.iter().enumerate().map(|(vp_index, cpu)| {
            let mpidr = aarch64defs::MpidrEl1::from(
                cpu.reg & u64::from(aarch64defs::MpidrEl1::AFFINITY_MASK),
            )
            .with_res1_31(true)
            .with_u(cpus.len() == 1);
            vm_topology::processor::aarch64::Aarch64VpInfo {
                base: VpInfo {
                    vp_index: VpIndex::new(vp_index as u32),
                    vnode: cpu.vnode,
                },
                mpidr,
                gicr: gic_redistributors_base
                    + vp_index as u64 * aarch64defs::GIC_REDISTRIBUTOR_SIZE,
                pmu_gsiv,
            }
        }))
        .context("failed to construct the processor topology")
}

/// Test guest memory access to sanity check that the expected ranges are
/// accessible.
fn guest_memory_access_self_test(
    mem_layout: &MemoryLayout,
    is_restoring: bool,
    create_partition_available: bool,
    highest_vtl_gm: &GuestMemory,
    shared_pool: &[MemoryRangeWithNode],
    vtom: Option<u64>,
) -> anyhow::Result<()> {
    tracing::info!(CVM_ALLOWED, "starting guest memory self test");

    // When restoring, and when running in a partition with create partitions
    // available, VTL0 may have donated pages to the hypervisor and those pages
    // are no longer accessible. Allow failure in those cases, but still run the
    // self test.
    let self_test_failure_allowed = is_restoring && create_partition_available;
    let test_gm = |accessible: bool, gpa: u64, error: String| {
        let res = highest_vtl_gm.read_plain::<u8>(gpa);
        if accessible && res.is_err() {
            if self_test_failure_allowed {
                tracing::warn!(
                    CVM_ALLOWED,
                    gpa = gpa,
                    "guest memory self test: RAM access failure allowed during restore: {}",
                    error
                );
                Ok(())
            } else {
                Err(anyhow::anyhow!(error))
            }
        } else if !accessible && res.is_ok() {
            Err(anyhow::anyhow!(error))
        } else {
            Ok(())
        }
    };
    for range in mem_layout.ram() {
        for gpa in [range.range.start(), range.range.end() - 1] {
            // Standard RAM is accessible.
            test_gm(true, gpa, format!("failed to read RAM at {gpa:#x}"))?;

            // It is not initially accessible above VTOM.
            //
            // FUTURE: When we support servicing on isolated guests, this may
            // also need to be allowed to fail, as a guest may have changed
            // visibility of pages.
            if let Some(vtom) = vtom {
                test_gm(
                    false,
                    gpa | vtom,
                    format!("RAM at {gpa:#x} is accessible above VTOM"),
                )?;
            }
        }
    }

    for range in shared_pool {
        let gpa = range.range.start();
        // Shared RAM is not accessible below VTOM.
        test_gm(
            false,
            gpa,
            format!("shared RAM at {gpa:#x} is accessible below VTOM"),
        )?;

        // But it is accessible above VTOM.
        if let Some(vtom) = vtom {
            test_gm(
                true,
                gpa | vtom,
                format!("failed to read shared RAM at {gpa:#x} above VTOM"),
            )?;
        }
    }
    tracing::info!(CVM_ALLOWED, "guest memory self test complete");

    Ok(())
}

/// Run the underhill specific worker entrypoint.
async fn new_underhill_vm(
    get_spawner: impl Spawn,
    tp: &AffinitizedThreadpool,
    early_init_driver: LocalDriver,
    params: UhVmParams,
) -> anyhow::Result<LoadedVm> {
    let UhVmParams {
        mut get_client,
        mut dps,
        servicing_state,
        boot_init,
        env_cfg,
        remote_console_cfg,
        debugger_rpc,
        control_send,
    } = params;

    if let Ok(kernel_boot_time) = std::env::var("KERNEL_BOOT_TIME") {
        if let Ok(kernel_boot_time_ns) = kernel_boot_time.parse::<u64>() {
            tracing::info!(CVM_ALLOWED, kernel_boot_time_ns, "kernel boot time");
        }
    }

    // Read the initial configuration from the IGVM parameters.
    let (runtime_params, measured_vtl2_info) =
        crate::loader::vtl2_config::read_vtl2_params().context("failed to read load parameters")?;

    // Log information about VTL2 memory
    let memory_allocation_mode = runtime_params.parsed_openhcl_boot().memory_allocation_mode;
    tracing::info!(
        CVM_ALLOWED,
        ?memory_allocation_mode,
        "memory allocation mode"
    );
    tracing::info!(
        CVM_ALLOWED,
        vtl2_ram = runtime_params
            .vtl2_memory_map()
            .iter()
            .map(|r| r.range.to_string())
            .collect::<Vec<String>>()
            .join(", "),
        "vtl2 ram"
    );

    let isolation = match runtime_params.parsed_openhcl_boot().isolation {
        bootloader_fdt_parser::IsolationType::None => virt::IsolationType::None,
        bootloader_fdt_parser::IsolationType::Vbs => virt::IsolationType::Vbs,
        bootloader_fdt_parser::IsolationType::Snp => virt::IsolationType::Snp,
        bootloader_fdt_parser::IsolationType::Tdx => virt::IsolationType::Tdx,
    };

    let hardware_isolated = isolation.is_hardware_isolated();

    // Temporarily override the host provided default_boot_always_attempt
    // value for non-Trusted Launch VMs until all hosts in Azure have been
    // updated to provide the correct value.
    //
    // Trusted Launch is roughly equivalent to having secure boot and TPM
    // enabled. For VMs that are not Trusted Launch, default boot is necessary
    // because the VMGS is not swapped with the OS disk in Azure (and in any
    // case on-prem), causing the VM to fail to boot after an OS swap.
    //
    // TODO: remove this (and petri workaround) once host changes are saturated
    if !isolation.is_isolated()
        && !dps.general.secure_boot_enabled
        && !dps.general.tpm_enabled
        && !dps.general.default_boot_always_attempt
    {
        tracing::info!("overriding dps to enable default_boot_always_attempt");
        dps.general.default_boot_always_attempt = true;
    }

    // override dps values with env_cfg values as necessary
    let dps = {
        if let Some(value) = env_cfg.disable_uefi_frontpage {
            tracing::info!("using OPENHCL_DISABLE_UEFI_FRONTPAGE={value} from cmdline");
            dps.general.disable_frontpage = value;
        }

        if let Some(value) = env_cfg.default_boot_always_attempt {
            tracing::info!("using HCL_DEFAULT_BOOT_ALWAYS_ATTEMPT={value} from cmdline");
            dps.general.default_boot_always_attempt = value;
        }

        if let Some(lifetime) = env_cfg.guest_state_lifetime {
            tracing::info!("using HCL_GUEST_STATE_LIFETIME={lifetime:?} from cmdline");
            dps.general.guest_state_lifetime = match lifetime {
                GuestStateLifetimeCli::Default => GuestStateLifetime::Default,
                GuestStateLifetimeCli::ReprovisionOnFailure => {
                    GuestStateLifetime::ReprovisionOnFailure
                }
                GuestStateLifetimeCli::Reprovision => GuestStateLifetime::Reprovision,
                GuestStateLifetimeCli::Ephemeral => GuestStateLifetime::Ephemeral,
            };
        }

        if env_cfg.reformat_vmgs {
            dps.general.guest_state_lifetime = GuestStateLifetime::Reprovision;
        }

        if let Some(policy) = env_cfg.guest_state_encryption_policy {
            tracing::info!("using HCL_GUEST_STATE_ENCRYPTION_POLICY={policy:?} from cmdline");
            dps.general.guest_state_encryption_policy = match policy {
                GuestStateEncryptionPolicyCli::Auto => GuestStateEncryptionPolicy::Auto,
                GuestStateEncryptionPolicyCli::GspById => GuestStateEncryptionPolicy::GspById,
                GuestStateEncryptionPolicyCli::GspKey => GuestStateEncryptionPolicy::GspKey,
                GuestStateEncryptionPolicyCli::None => GuestStateEncryptionPolicy::None,
            };
        }

        if let Some(value) = env_cfg.strict_encryption_policy {
            tracing::info!("using HCL_STRICT_ENCRYPTION_POLICY={value} from cmdline");
            dps.general
                .management_vtl_features
                .set_strict_encryption_policy(value);
        }

        if let Some(value) = env_cfg.attempt_ak_cert_callback {
            tracing::info!("using HCL_ATTEMPT_AK_CERT_CALLBACK={value} from cmdline");
            dps.general
                .management_vtl_features
                .set_control_ak_cert_provisioning(true);
            dps.general
                .management_vtl_features
                .set_attempt_ak_cert_callback(value);
        }

        dps
    };

    let driver_source = VmTaskDriverSource::new(ThreadpoolBackend::new(tp.clone()));

    let is_restoring = servicing_state.is_some();
    let servicing_state = OptionServicingInitState::from(servicing_state);

    assert!(
        !(is_restoring && isolation.is_isolated()),
        "restoring an isolated VM is not yet supported"
    );

    if let Some(Some(servicing::FlushLogsResult { duration_us, error })) =
        servicing_state.flush_logs_result
    {
        if let Some(error) = error {
            tracing::error!(CVM_ALLOWED, duration_us, error, "flush logs result")
        } else {
            tracing::info!(CVM_ALLOWED, duration_us, "flush logs result")
        }
    }

    let uevent_listener =
        Arc::new(UeventListener::new(tp.driver(0)).context("failed to start uevent listener")?);

    let use_mmio_hypercalls = dps.general.always_relay_host_mmio;
    // TODO: Centralize cpuid based feature determination.
    #[cfg(guest_arch = "x86_64")]
    let use_mmio_hypercalls = use_mmio_hypercalls
        || hardware_isolated && {
            let result =
                safe_intrinsics::cpuid(hvdef::HV_CPUID_FUNCTION_MS_HV_ENLIGHTENMENT_INFORMATION, 0);
            hvdef::HvEnlightenmentInformation::from(
                result.eax as u128
                    | (result.ebx as u128) << 32
                    | (result.ecx as u128) << 64
                    | (result.edx as u128) << 96,
            )
            .use_hypercall_for_mmio_access()
        };

    let boot_info = runtime_params.parsed_openhcl_boot();

    // The amount of memory required by the GET igvm_attest request
    let attestation = get_protocol::IGVM_ATTEST_MSG_MAX_SHARED_GPA as u64 * hvdef::HV_PAGE_SIZE;

    // TODO: retrieve this via the host; this heuristic is temporary.
    // Each MANA queue uses 21 pages.  Max 32 queues from OpenHCL (unless overridden by Vtl2Settings).
    let net_device_dma = 21 * hvdef::HV_PAGE_SIZE * (boot_info.cpus.len() as u64).min(32);
    // Each NVMe queue uses 130 pages.  While this can be set independently in policy via Vtl2Settings,
    // not expected to scale beyond VP count.  Max 128 queues from container policy.
    let nvme_device_dma = 130 * hvdef::HV_PAGE_SIZE * (boot_info.cpus.len() as u64).min(128);
    // Support up to 8 devices for each
    let device_dma = net_device_dma * 8 + nvme_device_dma * 8;

    // Determine the amount of shared memory to reserve from VTL0.
    let shared_pool_size = match isolation {
        #[cfg(guest_arch = "x86_64")]
        virt::IsolationType::Snp => {
            let cpu_bytes = boot_info.cpus.len() as u64
                * virt_mshv_vtl::SnpBacked::shared_pages_required_per_cpu()
                * hvdef::HV_PAGE_SIZE;

            round_up_to_2mb(cpu_bytes + device_dma + attestation)
        }
        #[cfg(guest_arch = "x86_64")]
        virt::IsolationType::Tdx => {
            let cpu_bytes = boot_info.cpus.len() as u64
                * virt_mshv_vtl::TdxBacked::shared_pages_required_per_cpu()
                * hvdef::HV_PAGE_SIZE;

            round_up_to_2mb(cpu_bytes + device_dma + attestation)
        }
        virt::IsolationType::Vbs => round_up_to_2mb(device_dma + attestation),
        _ => 0,
    };

    // Construct the VTL0 memory map by filtering out non-VTL0 ranges.
    let vtl0_memory_map = runtime_params
        .partition_memory_map()
        .iter()
        .filter_map(|entry| match entry {
            bootloader_fdt_parser::AddressRange::Memory(memory) => {
                if memory.vtl_usage == MemoryVtlType::VTL0 {
                    Some((memory.range.clone(), memory.igvm_type))
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    let hide_isolation = isolation.is_isolated() && env_cfg.hide_isolation;

    let mut with_vmbus: bool = false;
    let mut with_vmbus_relay = false;
    if dps.general.vmbus_redirection_enabled {
        with_vmbus = true;
        // If the guest is isolated but we are hiding this fact, then don't
        // start the relay--the guest will not be able to use relayed channels
        // since it will not be able to put their ring buffers in shared memory.
        with_vmbus_relay = !hide_isolation;
    }

    let enable_vpci_relay = env_cfg
        .enable_vpci_relay
        .unwrap_or(hardware_isolated && !hide_isolation && with_vmbus_relay);

    if enable_vpci_relay && !with_vmbus_relay {
        anyhow::bail!("cannot run the VPCI relay without the VMBus relay");
    }

    let mut vtl0_mmio;
    let (vtl0_mmio, vpci_relay_mmio) = if enable_vpci_relay {
        // Carve out enough VTL0 MMIO space for 64 devices.
        let required_len = 64 * vpci_relay::VPCI_RELAY_MMIO_PER_DEVICE;
        vtl0_mmio = boot_info.vtl0_mmio.clone();
        if vtl0_mmio.last().is_none_or(|r| r.len() < required_len) {
            anyhow::bail!("too little VTL0 MMIO space to take for the VPCI relay");
        }
        let r = vtl0_mmio.last().unwrap();
        let (rest, vpci) = r.split_at_offset(r.len() - required_len);
        *vtl0_mmio.last_mut().unwrap() = rest;
        (&vtl0_mmio, vpci)
    } else {
        (&boot_info.vtl0_mmio, MemoryRange::EMPTY)
    };

    let BuiltVtl0MemoryLayout {
        vtl0_memory_map,
        vtl0_memory_layout: mem_layout,
        shared_pool,
        complete_memory_layout,
    } = build_vtl0_memory_layout(vtl0_memory_map, vtl0_mmio, shared_pool_size)?;

    // Determine if x2apic is supported so that the topology matches
    // reality.
    //
    // We don't know if x2apic is forced on, but currently it doesn't really
    // matter because the topology's initial x2apic state is not currently
    // used in Underhill.
    //
    // FUTURE: consider having Underhill decide whether x2apic is enabled at
    // boot rather than allowing the host to make that decision. This would
    // just require Underhill setting the apicbase register on the VPs
    // before start.
    //
    // TODO: centralize cpuid querying logic.
    #[cfg(guest_arch = "x86_64")]
    let x2apic = if isolation.is_hardware_isolated() && !hide_isolation {
        // For hardware CVMs, always enable x2apic support at boot.
        vm_topology::processor::x86::X2ApicState::Enabled
    } else {
        let features = x86defs::cpuid::VersionAndFeaturesEcx::from(
            safe_intrinsics::cpuid(x86defs::cpuid::CpuidFunction::VersionAndFeatures.0, 0).ecx,
        );
        if features.x2_apic() {
            vm_topology::processor::x86::X2ApicState::Supported
        } else {
            vm_topology::processor::x86::X2ApicState::Unsupported
        }
    };

    #[cfg(guest_arch = "x86_64")]
    let processor_topology = new_x86_topology(&boot_info.cpus, x2apic)
        .context("failed to construct the processor topology")?;

    #[cfg(guest_arch = "aarch64")]
    let processor_topology = {
        new_aarch64_topology(
            boot_info
                .gic
                .context("did not get gic state from bootloader")?,
            &boot_info.cpus,
            boot_info
                .pmu_gsiv
                .context("did not get pmu gsiv from bootloader")?,
        )
        .context("failed to construct the processor topology")?
    };

    // also construct the VMGS nice and early, as much like the GET, it also
    // plays an important role during initial bringup
    let mut vmgs = match (dps.general.guest_state_lifetime, servicing_state.vmgs) {
        (GuestStateLifetime::Ephemeral, _) => None,
        (_, Some((vmgs_state, vmgs_get_meta_state))) => {
            // fast path, with zero .await calls
            let disk = disk_get_vmgs::GetVmgsDisk::restore_with_meta(
                get_client.clone(),
                vmgs_get_meta_state,
            )
            .context("failed to open VMGS disk")?;
            Some((
                disk.save_meta(),
                Vmgs::open_from_saved(
                    Disk::new(disk).context("invalid vmgs disk")?,
                    vmgs_state,
                    Some(Arc::new(GetVmgsLogger::new(get_client.clone()))),
                ),
            ))
        }
        (_, None) => {
            let disk = disk_get_vmgs::GetVmgsDisk::new(get_client.clone())
                .instrument(tracing::info_span!("vmgs_get_storage", CVM_ALLOWED))
                .await
                .context("failed to get VMGS client")?;

            let meta = disk.save_meta();
            let disk = Disk::new(disk).context("invalid vmgs disk")?;
            let logger = Arc::new(GetVmgsLogger::new(get_client.clone()));

            let vmgs = if matches!(
                dps.general.guest_state_lifetime,
                GuestStateLifetime::Reprovision
            ) {
                Vmgs::request_format(disk, Some(logger))
                    .instrument(tracing::info_span!("vmgs_format", CVM_ALLOWED))
                    .await
                    .context("failed to format vmgs")?
            } else {
                Vmgs::try_open(
                    disk,
                    Some(logger),
                    !is_restoring,
                    matches!(
                        dps.general.guest_state_lifetime,
                        GuestStateLifetime::ReprovisionOnFailure
                    ),
                )
                .instrument(tracing::info_span!("vmgs_open", CVM_ALLOWED))
                .await
                .context("failed to open vmgs")?
            };

            Some((meta, vmgs))
        }
    };

    // Determine if the VTL0 alias map is in use.
    let vtl0_alias_map_bit =
        runtime_params
            .parsed_openhcl_boot()
            .vtl0_alias_map
            .filter(|&alias_map| {
                // TODO: Kernel won't support bits greater than 48. Need 5 level paging
                //       or some other kernel changes. If possible, would be good to not
                //       require 5 level paging and just further extend valid bits.
                if alias_map <= 1 << 48 {
                    tracing::info!(CVM_ALLOWED, alias_map, "enabling alias map");
                    true
                } else {
                    // BUGBUG: This needs to be fixed, but allow it with just an error
                    // log for now.
                    tracing::error!(
                        CVM_ALLOWED,
                        alias_map,
                        "alias map bit larger than supported"
                    );
                    false
                }
            });

    let vtom = measured_vtl2_info.vtom_offset_bit.map(|n| 1u64 << n);

    assert_eq!(
        vtom.is_some(),
        hardware_isolated,
        "vtom must be present if and only if hardware isolation is enabled"
    );

    // Construct the underhill partition instance. This contains much of the configuration of the guest deposited by
    // the host, along with additional device configuration and transports.
    let params = UhPartitionNewParams {
        lower_vtl_memory_layout: &mem_layout,
        isolation,
        topology: &processor_topology,
        cvm_cpuid_info: runtime_params.cvm_cpuid_info(),
        snp_secrets: runtime_params.snp_secrets(),
        vtom,
        handle_synic: with_vmbus,
        no_sidecar_hotplug: env_cfg.no_sidecar_hotplug,
        use_mmio_hypercalls,
        intercept_debug_exceptions: env_cfg.gdbstub,
        hide_isolation,
        disable_proxy_redirect: env_cfg.disable_proxy_redirect,
        disable_lower_vtl_timer_virt: env_cfg.disable_lower_vtl_timer_virt,
    };

    let proto_partition = UhProtoPartition::new(params, |cpu| tp.driver(cpu).clone())
        .context("failed to create prototype partition")?;

    let gm = underhill_mem::init(&underhill_mem::Init {
        processor_topology: &processor_topology,
        isolation,
        vtl0_alias_map_bit,
        vtom,
        mem_layout: &mem_layout,
        complete_memory_layout: &complete_memory_layout,
        boot_init: boot_init.then_some(underhill_mem::BootInit {
            tp,
            vtl2_memory: runtime_params.vtl2_memory_map(),
            accepted_regions: measured_vtl2_info.accepted_regions(),
        }),
        shared_pool: &shared_pool,
        maximum_vtl: if proto_partition.guest_vsm_available() {
            Vtl::Vtl1
        } else {
            Vtl::Vtl0
        },
    })
    .await
    .context("failed to initialize memory")?;

    // Devices in hardware isolated VMs default to accessing only shared memory,
    // since that is what the guest expects--it will double buffer memory to be
    // DMAed through a shared memory pool.
    //
    // When hiding isolation, allow devices to access all memory, since that's
    // the only option: the guest won't and can't transition anything to shared.
    //
    // For non-isolated VMs, there is no shared/private distinction, so devices
    // access the same memory as the guest. For software-isolated VMs, the
    // hypervisor does not allow the paravisor to observe changes to
    // shared/private state, so we have no choice but to allow devices to access
    // both.
    let device_memory = if hide_isolation || !isolation.is_hardware_isolated() {
        gm.vtl0()
    } else {
        &gm.cvm_memory().unwrap().shared_gm
    };

    let mut dma_manager = OpenhclDmaManager::new(
        &shared_pool.iter().map(|r| r.range).collect::<Vec<_>>(),
        &runtime_params
            .private_pool_ranges()
            .iter()
            .map(|r| r.range)
            .collect::<Vec<_>>(),
        measured_vtl2_info
            .vtom_offset_bit
            .map(|bit| 1 << bit)
            .unwrap_or(0),
        isolation,
    )
    .context("failed to create global dma manager")?;

    if let Some(dma_manager_state) = servicing_state.dma_manager_state.flatten() {
        use vmcore::save_restore::SaveRestore;
        dma_manager
            .restore(dma_manager_state)
            .context("failed to restore global dma manager")?;
    }

    // Test with the highest VTL for which we have a GuestMemory object
    let highest_vtl_gm = gm.vtl1().unwrap_or(gm.vtl0());

    // Perform a quick validation to make sure each range is appropriately
    // accessible.
    guest_memory_access_self_test(
        &mem_layout,
        is_restoring,
        proto_partition.create_partition_available(),
        highest_vtl_gm,
        &shared_pool,
        vtom,
    )
    .context("guest memory access self test failed")?;

    // Set the gpa allocator to GET that is required by the attestation message.
    get_client.set_gpa_allocator(
        dma_manager
            .new_client(DmaClientParameters {
                device_name: "get".into(),
                lower_vtl_policy: LowerVtlPermissionPolicy::Vtl0,
                allocation_visibility: if isolation.is_isolated() {
                    AllocationVisibility::Shared
                } else {
                    AllocationVisibility::Private
                },
                persistent_allocations: false,
            })
            .context("get dma client")?,
    );

    if confidential_debug_enabled() {
        tracing::warn!(CVM_ALLOWED, "confidential debug enabled");
    }

    // Create the `AttestationVmConfig` from `dps`, which will be used in
    // - stateful mode (the attestation is not suppressed)
    // - stateless mode (isolated VM with attestation suppressed)
    let attestation_vm_config = AttestationVmConfig {
        current_time: None,
        // TODO CVM: Support vmgs provisioning config
        root_cert_thumbprint: String::new(),
        console_enabled: dps.general.com1_enabled
            || dps.general.com2_enabled
            || dps.general.com1_vmbus_redirector
            || dps.general.com2_vmbus_redirector,
        secure_boot: dps.general.secure_boot_enabled,
        tpm_enabled: dps.general.tpm_enabled,
        tpm_persisted: !dps.general.suppress_attestation.unwrap_or(false),
        filtered_vpci_devices_allowed: with_vmbus_relay
            && dps.general.vpci_boot_enabled
            && isolation.is_isolated(),
        vm_unique_id: dps.general.bios_guid.to_string(),
    };

    let tee_call: Option<Box<dyn tee_call::TeeCall>> = match isolation {
        virt::IsolationType::Snp => Some(Box::new(tee_call::SnpCall)),
        virt::IsolationType::Tdx => Some(Box::new(tee_call::TdxCall)),
        virt::IsolationType::Vbs => Some(Box::new(tee_call::VbsCall)),
        virt::IsolationType::None => None,
    };

    // Decrypt VMGS state before the VMGS file is used for anything.
    //
    // `refresh_tpm_seeds` is a host side GSP service configuration
    // that is passed to vTPM.
    // `agent_data` and `guest_secret_key` may also be used by vTPM
    // initialization.
    let platform_attestation_data = {
        if !is_restoring && let Some(vmgs) = vmgs.as_mut() {
            // Perform attestation by calling `initialize_platform_security`. This
            // will unlock the VMGS file internally.
            // Note that the routine will make callouts to the host via GET and receive
            // responses via shared memory, which requires both `shared_vis_pages_pool` and
            // `gm.untrusted_dma_memory` to be available.
            let suppress_attestation = dps.general.suppress_attestation.unwrap_or_default();
            if isolation.is_isolated() {
                validate_isolated_configuration(&dps)
                    .context("invalid host-provided configuration for isolated VM")?;
            }
            underhill_attestation::initialize_platform_security(
                &get_client,
                dps.general.bios_guid,
                &attestation_vm_config,
                &mut vmgs.1,
                tee_call.as_deref(),
                suppress_attestation,
                early_init_driver,
                dps.general.guest_state_encryption_policy,
                dps.general
                    .management_vtl_features
                    .strict_encryption_policy(),
            )
            .instrument(tracing::info_span!(
                "initialize_platform_security",
                CVM_ALLOWED
            ))
            .await
            .context("failed to initialize platform security")?
        } else {
            // TODO CVM: Save and restore last returned data when live servicing is supported.
            // We also need to revisit what states should be saved and restored.
            //
            // If this is an Underhill restart, the VMGS has already been
            // restored in its unlocked state.
            underhill_attestation::PlatformAttestationData {
                host_attestation_settings: underhill_attestation::HostAttestationSettings {
                    refresh_tpm_seeds: false,
                },
                agent_data: None,
                guest_secret_key: None,
            }
        }
    };

    let mut resolver = ResourceResolver::new();
    // Make the GET available for other resources.
    resolver.add_resolver(get_client.clone());

    let (vmgs_client, vmgs) = if let Some((meta, vmgs)) = vmgs {
        // Spawn the VMGS client for multi-task access.
        let (vmgs_client, vmgs_handle) = spawn_vmgs_broker(get_spawner, vmgs);
        resolver.add_resolver(vmgs_client.clone());
        (
            Some(Box::new(vmgs_client.clone()) as Box<dyn VmgsBrokerNonVolatileStore>),
            Some((vmgs_client, meta, vmgs_handle)),
        )
    } else {
        (None, None)
    };

    // Enable remote chipset devices.
    resolver.add_async_resolver(
        chipset_device_worker::resolver::RemoteChipsetDeviceResolver(
            OpenHclRemoteDynamicResolvers {
                get: get_client.clone(),
                vmgs: vmgs.as_ref().map(|(client, _, _)| client.clone()),
            },
        ),
    );

    // Read measured config from VTL0 memory. When restoring, it is already gone.
    let (firmware_type, measured_vtl0_info, load_kind) = {
        if let Some(firmware_type) = servicing_state.firmware_type {
            (firmware_type.into(), None, LoadKind::None)
        } else {
            let config = MeasuredVtl0Info::read_from_memory(gm.vtl0())
                .context("failed to read measured vtl0 info")?;
            let load_kind = if let Some(kind) = env_cfg.force_load_vtl0_image {
                tracing::info!(CVM_ALLOWED, kind, "overriding dps load type");
                match kind.as_str() {
                    "pcat" => LoadKind::Pcat,
                    "uefi" => LoadKind::Uefi,
                    "linux" => LoadKind::Linux,
                    _ => anyhow::bail!("unexpected force load vtl0 type {kind}"),
                }
            } else {
                if dps.general.firmware_mode_is_pcat {
                    LoadKind::Pcat
                } else {
                    LoadKind::Uefi
                }
            };

            let firmware_type: FirmwareType = load_kind.into();
            (firmware_type, Some(config), load_kind)
        }
    };

    // Only advertise extended IOAPIC on non-PCAT systems.
    #[cfg(guest_arch = "x86_64")]
    let cpuid = {
        let confidential_vmbus = isolation.is_hardware_isolated() && with_vmbus_relay;
        let extended_ioapic_rte = !matches!(firmware_type, FirmwareType::Pcat);
        vmm_core::cpuid::hyperv_cpuid_leaves(extended_ioapic_rte, confidential_vmbus)
            .collect::<Vec<_>>()
    };

    let (crash_notification_send, crash_notification_recv) = mesh::channel();

    let mut state_units = StateUnits::new();

    // Process VM time timers on VP 0, since that's where most of the
    // vmtime-driven device interrupts will be triggered.
    let vmtime_keeper = VmTimeKeeper::new(tp.driver(0), VmTime::from_100ns(0));
    let vmtime_source = vmtime_keeper.builder().build(tp.driver(0)).await.unwrap();
    let vmtime = state_units
        .add("vmtime")
        .spawn(&tp, |recv| {
            let mut vmtime = vmtime_keeper;
            async move {
                run_vmtime(&mut vmtime, recv).await;
                vmtime
            }
        })
        .unwrap();

    let cvm_params = if isolation.is_hardware_isolated() {
        let cvm_mem = gm.cvm_memory().unwrap();
        Some(virt_mshv_vtl::CvmLateParams {
            shared_gm: cvm_mem.shared_gm.clone(),
            isolated_memory_protector: cvm_mem.protector.clone(),
            shared_dma_client: dma_manager.new_client(DmaClientParameters {
                device_name: "partition-shared".into(),
                lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                allocation_visibility: AllocationVisibility::Shared,
                persistent_allocations: false,
            })?,
            private_dma_client: dma_manager.new_client(DmaClientParameters {
                device_name: "partition-private".into(),
                lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                allocation_visibility: AllocationVisibility::Private,
                persistent_allocations: false,
            })?,
        })
    } else {
        None
    };

    let late_params = virt_mshv_vtl::UhLateParams {
        gm: [
            gm.vtl0().clone(),
            gm.vtl1().cloned().unwrap_or(GuestMemory::empty()),
        ]
        .into(),
        vtl0_kernel_exec_gm: gm.vtl0_kernel_execute().clone(),
        vtl0_user_exec_gm: gm.vtl0_user_execute().clone(),
        #[cfg(guest_arch = "x86_64")]
        cpuid,
        crash_notification_send,
        vmtime: &vmtime_source,
        cvm_params,
        vmbus_relay: with_vmbus_relay,
    };

    let (partition, vps) = proto_partition
        .build(late_params)
        .instrument(tracing::info_span!("new_uh_partition", CVM_ALLOWED))
        .await
        .context("failed to create partition")?;

    let partition = Arc::new(partition);

    // By default, scale the max QD by the number of VPs to save memory
    // on smaller VMs, up to a QD of 256.
    // Smaller VMs have lower performance targets than larger VMs,
    // so they don't need as high a QD.
    let default_io_queue_depth = (8 * processor_topology.vp_count()).min(256);

    let mut controllers = InitialControllers::new(
        &uevent_listener,
        &dps,
        env_cfg.nvme_vfio,
        is_restoring,
        default_io_queue_depth,
        env_cfg.config_timeout_in_seconds,
    )
    .instrument(tracing::info_span!("new_initial_controllers", CVM_ALLOWED))
    .await
    .context("failed to merge configuration")?;

    // TODO MCR: support closed-source configuration logic for MCR device
    if env_cfg.mcr {
        use crate::dispatch::vtl2_settings_worker::UhVpciDeviceConfig;
        tracing::info!(CVM_ALLOWED, "Instantiating The MCR Device");
        const MCR_INSTANCE_ID: Guid = guid::guid!("07effd8f-7501-426c-a947-d8345f39113d");

        let res = UhVpciDeviceConfig {
            instance_id: MCR_INSTANCE_ID,
            resource: mcr_resources::McrControllerHandle {
                instance_id: MCR_INSTANCE_ID,
            }
            .into_resource(),
        };
        controllers.vpci_devices.push(res);
    } else {
        tracing::info!(CVM_ALLOWED, "Not Instantiating The MCR Device");
    }

    let (halt_vps, halt_request_recv) = Halt::new();
    let halt_vps = Arc::new(halt_vps);

    resolver.add_resolver(vmm_core::platform_resolvers::HaltResolver(halt_vps.clone()));

    let bounce_buffer_tracker = {
        let size = {
            if let Some(vtl2_settings) = dps.general.vtl2_settings.as_ref() {
                vtl2_settings.fixed.max_bounce_buffer_pages.unwrap_or(2048)
            } else {
                // 8Mb maximum which is the Azure limit for unaligned IOs
                2048
            }
        } as _;

        Arc::new(scsi_buffers::BounceBufferTracker::new(
            size,
            processor_topology.vp_count() as usize,
        ))
    };

    // ARM64 always bounces, as the OpenHCL kernel does not have access to VTL0
    // pages. Necessary until #273 is resolved.
    //
    // Currently we always bounce for CVM as well, due to underhill_mem not
    // supporting registering shared or private memory with the kernel.
    let always_bounce = cfg!(guest_arch = "aarch64") || isolation.is_hardware_isolated();
    resolver.add_async_resolver::<DiskHandleKind, _, OpenBlockDeviceConfig, _>(
        BlockDeviceResolver::new(
            Arc::new(tp.clone()),
            Some(uevent_listener.clone()),
            bounce_buffer_tracker,
            always_bounce,
        ),
    );

    let periodic_telemetry_task = tp.spawn(
        "periodic_telemetry_collection",
        crate::inspect_proc::periodic_telemetry_task(driver_source.simple()),
    );

    let nvme_manager = if env_cfg.nvme_vfio {
        // TODO: reevaluate enablement of nvme save restore when private pool
        // save restore to bootshim is available.
        let private_pool_available = !runtime_params.private_pool_ranges().is_empty();
        let save_restore_supported = env_cfg.nvme_keep_alive.is_enabled() && private_pool_available;

        let manager = NvmeManager::new(
            &driver_source,
            processor_topology.vp_count(),
            save_restore_supported,
            servicing_state.nvme_state.unwrap_or(None),
            Arc::new(VfioNvmeDriverSpawner {
                nvme_always_flr: env_cfg.nvme_always_flr,
                is_isolated: isolation.is_isolated(),
                dma_client_spawner: dma_manager.client_spawner(),
            }),
        );

        tracing::debug!(
            CVM_ALLOWED,
            nvme_vfio = true,
            save_restore_supported,
            "NVMe VFIO manager initialized, setting up resolver"
        );
        resolver.add_async_resolver::<DiskHandleKind, _, NvmeDiskConfig, _>(NvmeDiskResolver::new(
            manager.client().clone(),
        ));

        Some(manager)
    } else {
        None
    };

    let initial_generation_id = match dps.general.generation_id.map(u128::from_ne_bytes) {
        Some(0) | None => {
            let mut gen_id = [0; 16];
            tracing::trace!("Generation ID uninitialized by host.");
            getrandom::fill(&mut gen_id).expect("rng failure");
            gen_id
        }
        Some(n) => n.to_ne_bytes(),
    };

    // TODO: move to instantiate via a resource.
    let rtc_time_source = ArcMutexUnderhillLocalClock(Arc::new(Mutex::new(
        UnderhillLocalClock::new(
            get_client.clone(),
            if let Some(vmgs_client) = vmgs_client.as_ref() {
                vmgs_client
                    .as_non_volatile_store(vmgs::FileId::RTC_SKEW, false)
                    .context("failed to instantiate RTC skew store")?
            } else {
                EphemeralNonVolatileStore::new_boxed()
            },
            servicing_state.emuplat.rtc_local_clock,
        )
        .await
        .context("failed to initialize UnderhillLocalClock emuplat")?,
    )));

    #[cfg(guest_arch = "x86_64")]
    let mut deps_hyperv_firmware_pcat = None;
    #[cfg(not(guest_arch = "x86_64"))]
    let deps_hyperv_firmware_pcat = None;

    let mut deps_hyperv_firmware_uefi = None;
    match firmware_type {
        #[cfg(not(guest_arch = "x86_64"))]
        FirmwareType::Pcat => {
            panic!("Not supported");
        }
        #[cfg(guest_arch = "x86_64")]
        FirmwareType::Pcat => {
            let acpi_builder = AcpiTablesBuilder {
                processor_topology: &processor_topology,
                mem_layout: &mem_layout,
                cache_topology: None,
                pcie_host_bridges: &vec![],
                with_ioapic: true, // underhill always runs with ioapic
                with_pic: true,    // pcat always runs with pic and pit
                with_pit: true,
                with_psp: dps.general.psp_enabled,
                pm_base: PM_BASE,
                acpi_irq: SYSTEM_IRQ_ACPI,
            };

            let config = firmware_pcat::config::PcatBiosConfig {
                processor_topology: processor_topology.clone(),
                mem_layout: mem_layout.clone(),
                srat: acpi_builder.build_srat(),
                hibernation_enabled: dps.general.hibernation_enabled,
                initial_generation_id,
                boot_order: dps.general.pcat_boot_device_order.map(|e| {
                    use firmware_pcat::config::BootDevice;
                    use firmware_pcat::config::BootDeviceStatus;
                    use guest_emulation_transport::api::platform_settings::PcatBootDevice;

                    let kind = match e {
                        PcatBootDevice::Floppy => BootDevice::Floppy,
                        PcatBootDevice::Optical => BootDevice::Optical,
                        PcatBootDevice::HardDrive => BootDevice::HardDrive,
                        PcatBootDevice::Network => BootDevice::Network,
                    };

                    // TODO: Correctly mark if devices are attached or not.
                    BootDeviceStatus {
                        kind,
                        attached: true,
                    }
                }),
                num_lock_enabled: dps.general.num_lock_enabled,
                smbios: firmware_pcat::config::SmbiosConstants {
                    bios_guid: dps.general.bios_guid,
                    system_serial_number: dps.smbios.serial_number.clone(),
                    base_board_serial_number: (dps.smbios).base_board_serial_number.clone(),
                    chassis_serial_number: (dps.smbios).chassis_serial_number.clone(),
                    chassis_asset_tag: (dps.smbios).chassis_asset_tag.clone(),
                    bios_lock_string: dps.smbios.bios_lock_string.clone(),
                    processor_manufacturer: dps.smbios.processor_manufacturer.clone(),
                    processor_version: dps.smbios.processor_version.clone(),
                    cpu_info_bundle: Some(firmware_pcat::config::SmbiosProcessorInfoBundle {
                        processor_family: dps.smbios.processor_family2 as u8,
                        voltage: dps.smbios.voltage,
                        external_clock: dps.smbios.external_clock,
                        max_speed: dps.smbios.max_speed,
                        current_speed: dps.smbios.current_speed,
                    }),
                },
            };

            let halt_vps = halt_vps.clone();

            deps_hyperv_firmware_pcat = Some(dev::HyperVFirmwarePcat {
                config,
                logger: Box::new(UnderhillLogger {
                    get: get_client.clone(),
                }),
                generation_id_recv: get_client
                    .take_generation_id_recv()
                    .await
                    .context("failed to get generation ID channel")?,
                rom: None,
                replay_mtrrs: Box::new(move || halt_vps.replay_mtrrs()),
            })
        }
        FirmwareType::Uefi => {
            use firmware_uefi_custom_vars::CustomVars;
            use guest_emulation_transport::api::platform_settings::SecureBootTemplateType;
            use hcl_compat_uefi_nvram_storage::HclCompatNvram;
            use vmm_core::emuplat::hcl_compat_uefi_nvram_storage::VmgsStorageBackendAdapter;

            // map the GET's template enum onto the hardcoded secureboot template type
            let base_vars = match dps.general.secure_boot_template {
                SecureBootTemplateType::None => CustomVars::default(),
                SecureBootTemplateType::MicrosoftWindows => {
                    if cfg!(guest_arch = "x86_64") {
                        hyperv_secure_boot_templates::x64::microsoft_windows()
                    } else if cfg!(guest_arch = "aarch64") {
                        hyperv_secure_boot_templates::aarch64::microsoft_windows()
                    } else {
                        anyhow::bail!("no secure boot template for current guest_arch")
                    }
                }
                SecureBootTemplateType::MicrosoftUefiCertificateAuthority => {
                    if cfg!(guest_arch = "x86_64") {
                        hyperv_secure_boot_templates::x64::microsoft_uefi_ca()
                    } else if cfg!(guest_arch = "aarch64") {
                        hyperv_secure_boot_templates::aarch64::microsoft_uefi_ca()
                    } else {
                        anyhow::bail!("no secure boot template for current guest_arch")
                    }
                }
            };

            // check if vmgs includes custom UEFI JSON
            let custom_uefi_json_data = if let Some(vmgs_client) = vmgs_client.as_ref() {
                vmgs_client
                    .as_non_volatile_store(vmgs::FileId::CUSTOM_UEFI, false)
                    .context("failed to instantiate custom UEFI JSON store")?
                    .restore()
                    .await
                    .context("failed to get custom UEFI JSON data")?
            } else {
                None
            };

            // obtain the final custom uefi vars by applying the delta onto
            // the base vars
            let custom_uefi_vars = match custom_uefi_json_data {
                Some(data) => {
                    let res = (|| -> Result<CustomVars, anyhow::Error> {
                        let delta = hyperv_uefi_custom_vars_json::load_delta_from_json(&data)?;
                        Ok(base_vars.apply_delta(delta)?)
                    })();

                    match res {
                        Ok(vars) => vars,
                        Err(e) => {
                            tracing::error!(CVM_ALLOWED, "Failed to load custom UEFI vars");
                            get_client
                                .event_log_fatal(EventLogId::BOOT_FAILURE_SECURE_BOOT_FAILED)
                                .await;
                            return Err(e).context("failed to load custom UEFI variables");
                        }
                    }
                }
                None => base_vars,
            };

            let config = firmware_uefi::UefiConfig {
                custom_uefi_vars,
                secure_boot: dps.general.secure_boot_enabled,
                initial_generation_id,
                use_mmio: cfg!(not(guest_arch = "x86_64")),
                command_set: if cfg!(guest_arch = "x86_64") {
                    UefiCommandSet::X64
                } else {
                    UefiCommandSet::Aarch64
                },
                diagnostics_log_level: {
                    use get_protocol::dps_json::EfiDiagnosticsLogLevelType as LogLevelType;
                    let level = dps.general.efi_diagnostics_log_level.0;
                    match level {
                        x if x == LogLevelType::DEFAULT.0 => LogLevel::make_default(),
                        x if x == LogLevelType::INFO.0 => LogLevel::make_info(),
                        x if x == LogLevelType::FULL.0 => LogLevel::make_full(),
                        _ => LogLevel::make_default(),
                    }
                },
            };

            let (watchdog_send, watchdog_recv) = mesh::channel();
            deps_hyperv_firmware_uefi = Some(dev::HyperVFirmwareUefi {
                config,
                logger: Box::new(UnderhillLogger {
                    get: get_client.clone(),
                }),
                nvram_storage: if let Some(vmgs_client) = vmgs_client.as_ref() {
                    Box::new(HclCompatNvram::new(
                        VmgsStorageBackendAdapter(
                            vmgs_client
                                .as_non_volatile_store(vmgs::FileId::BIOS_NVRAM, true)
                                .context("failed to instantiate UEFI NVRAM store")?,
                        ),
                        Some(HclCompatNvramQuirks {
                            skip_corrupt_vars_with_missing_null_term: true,
                        }),
                    ))
                } else {
                    Box::new(uefi_nvram_storage::in_memory::InMemoryNvram::new())
                },
                generation_id_recv: get_client
                    .take_generation_id_recv()
                    .await
                    .expect("first time taking chan"),
                watchdog_platform: {
                    // UEFI watchdog doesn't persist to VMGS at this time
                    let store = EphemeralNonVolatileStore::new_boxed();

                    // Create base watchdog platform
                    let mut underhill_watchdog_platform =
                        UnderhillWatchdogPlatform::new(store, get_client.clone()).await?;

                    // Inject NMI on watchdog timeout
                    #[cfg(guest_arch = "x86_64")]
                    let watchdog_callback = WatchdogTimeoutNmi {
                        partition: partition.clone(),
                        watchdog_send: Some(watchdog_send),
                    };

                    // ARM64 does not have NMI support yet, so halt instead
                    #[cfg(guest_arch = "aarch64")]
                    let watchdog_callback = WatchdogTimeoutReset {
                        halt_vps: halt_vps.clone(),
                        watchdog_send: Some(watchdog_send),
                    };

                    // Add the callback
                    underhill_watchdog_platform.add_callback(Box::new(watchdog_callback));

                    Box::new(underhill_watchdog_platform)
                },
                watchdog_recv,
                vsm_config: Some(Box::new(UnderhillVsmConfig {
                    partition: Arc::downgrade(&partition),
                })),
                time_source: Box::new(rtc_time_source.new_linked_clock()),
            })
        }
        FirmwareType::None => {}
    };

    let mut serial_inputs = [None, None, None, None];

    if dps.general.com1_vmbus_redirector {
        serial_inputs[0] = Some(Resource::new(
            vmbus_serial_guest::OpenVmbusSerialGuestConfig::open(
                &vmbus_serial_guest::UART_INTERFACE_INSTANCE_COM1,
            )
            .context("failed to open com1")?,
        ));
    }

    if dps.general.com2_vmbus_redirector {
        serial_inputs[1] = Some(Resource::new(
            vmbus_serial_guest::OpenVmbusSerialGuestConfig::open(
                &vmbus_serial_guest::UART_INTERFACE_INSTANCE_COM2,
            )
            .context("failed to open com2")?,
        ));
    }

    let with_serial = serial_inputs.iter().any(|transport| transport.is_some());

    if dps.general.processor_idle_enabled {
        // TODO: Will likely address along with battery task above
        tracing::warn!(
            CVM_ALLOWED,
            "processor idle emulator unsupported for underhill"
        );
    }

    // Set the callback in GET to trigger the debug interrupt.
    let p = partition.clone();
    let debug_interrupt_callback = move |vtl: u8| p.assert_debug_interrupt(vtl);
    get_client.set_debug_interrupt_callback(Box::new(debug_interrupt_callback));

    let mut input_distributor = InputDistributor::new(remote_console_cfg.input);
    resolver.add_async_resolver::<KeyboardInputHandleKind, _, MultiplexedInputHandle, _>(
        input_distributor.client().clone(),
    );
    resolver.add_async_resolver::<MouseInputHandleKind, _, MultiplexedInputHandle, _>(
        input_distributor.client().clone(),
    );

    let input_distributor = state_units
        .add("input")
        .spawn(&tp, async |mut recv| {
            input_distributor.run(&mut recv).await;
            input_distributor
        })
        .unwrap();

    let mut ide_drives = [[None, None], [None, None]];
    let mut storvsp_ide_disks = Vec::new();
    let mut ide_io_queue_depth = None;

    if let Some(ide_config) = controllers.ide_controller {
        if firmware_type != FirmwareType::Pcat {
            anyhow::bail!("ide requires generation 1, VM is configured for generation 2");
        }

        ide_io_queue_depth = ide_config.io_queue_depth;

        for (channel, disks) in [
            (0, ide_config.primary_channel_disks),
            (1, ide_config.secondary_channel_disks),
        ] {
            for disk_cfg in disks.into_iter() {
                let drive = disk_cfg.path.drive;
                let media = match disk_cfg.guest_media {
                    GuestMedia::Dvd(device) => {
                        let scsi_dvd = resolver
                            .resolve(
                                device,
                                ResolveScsiDeviceHandleParams {
                                    driver_source: &driver_source,
                                },
                            )
                            .await?;
                        ide::DriveMedia::optical_disk(Arc::new(AtapiScsiDisk::new(scsi_dvd.0)))
                    }
                    GuestMedia::Disk {
                        disk_type,
                        read_only,
                        disk_parameters,
                    } => {
                        let disk =
                            disk_from_disk_type(disk_type, read_only, &resolver, &driver_source)
                                .await?;
                        let scsi_disk = Arc::new(scsidisk::SimpleScsiDisk::new(
                            disk.clone(),
                            disk_parameters.unwrap_or_default(),
                        ));

                        // Only disks, not DVD drives, get IDE accelerator channels.
                        storvsp_ide_disks.push((
                            IdePath { channel, drive },
                            ScsiControllerDisk::new(scsi_disk),
                        ));

                        ide::DriveMedia::hard_disk(disk)
                    }
                };

                let old_media = ide_drives[channel as usize]
                    .get_mut(drive as usize)
                    .context("invalid ide device")?
                    .replace(media);

                if old_media.is_some() {
                    anyhow::bail!("duplicate ide device at {}/{}", channel, drive);
                }
            }
        }
    }

    let emuplat_adjust_gpa_range;

    let synic = Arc::new(SynicPorts::new(partition.clone()));

    let mut chipset = vm_manifest_builder::VmManifestBuilder::new(
        match firmware_type {
            FirmwareType::Pcat => vm_manifest_builder::BaseChipsetType::HypervGen1,
            FirmwareType::Uefi => vm_manifest_builder::BaseChipsetType::HypervGen2Uefi,
            FirmwareType::None => vm_manifest_builder::BaseChipsetType::HyperVGen2LinuxDirect,
        },
        if cfg!(guest_arch = "x86_64") {
            vm_manifest_builder::MachineArch::X86_64
        } else if cfg!(guest_arch = "aarch64") {
            vm_manifest_builder::MachineArch::Aarch64
        } else {
            anyhow::bail!("unsupported guest architecture")
        },
    );

    if with_serial {
        chipset = chipset.with_serial(serial_inputs);
        if env_cfg.emulated_serial_wait_for_rts {
            chipset = chipset.with_serial_wait_for_rts();
        }
    }

    if matches!(firmware_type, FirmwareType::Pcat) {
        // Use the stub floppy implementation for compatibility with existing
        // releases and because we don't need a functional floppy disk.
        chipset = chipset.with_stub_floppy();
        // Use the host's VGA implementation, at least for now.
        chipset = chipset.with_proxy_vga();
    } else {
        if dps.general.watchdog_enabled {
            chipset = chipset.with_guest_watchdog();
        }

        if dps.general.psp_enabled {
            chipset = chipset.with_psp();
        }
    }

    if dps.general.battery_enabled {
        chipset = chipset.with_battery(
            get_client
                .take_battery_status_recv()
                .await
                .context("failed to get battery status channel")?,
        );
    }

    let vm_manifest_builder::VmChipsetResult {
        chipset,
        mut chipset_devices,
    } = chipset
        .build()
        .context("failed to build chipset configuration")?;

    let deps_generic_ioapic = chipset.with_generic_ioapic.then(|| dev::GenericIoApicDeps {
        num_entries: virt::irqcon::IRQ_LINES as u8,
        routing: Box::new(vmm_core::emuplat::ioapic::IoApicRouting(
            partition.ioapic_routing(),
        )),
    });

    use vmotherboard::options::dev;

    let pci_bus_id_piix4 = vmotherboard::BusId::new("i440bx");

    let deps_piix4_pci_bus = chipset.with_piix4_pci_bus.then(|| dev::Piix4PciBusDeps {
        bus_id: pci_bus_id_piix4.clone(),
    });

    let deps_i440bx_host_pci_bridge = if chipset.with_i440bx_host_pci_bridge {
        Some(dev::I440BxHostPciBridgeDeps {
            attached_to: pci_bus_id_piix4.clone(),
            adjust_gpa_range: {
                // TODO: improve slot range allocation, when there are more API consumers
                let base_slot = 0;
                // The host will only consider the first available RAM block when
                // creating PCAT mappings, so we need to do the same. This only
                // works because everybody keeps things sorted.
                const SIZE_1_MB: u64 = 1024 * 1024;
                const SIZE_4_GB: u64 = 4 * 1024 * SIZE_1_MB;
                let first_mem_block = &mem_layout.ram()[0];
                anyhow::ensure!(
                    first_mem_block.range.end() < SIZE_4_GB,
                    "first memory block must be below 4GB for adjust_gpa_range"
                );
                // Reserve 1MB off the top.
                let rom_bios_offset = first_mem_block.range.end() - SIZE_1_MB;

                let adjust_gpa_range = GetBackedAdjustGpaRange::new(
                    get_client.clone(),
                    base_slot,
                    rom_bios_offset,
                    servicing_state
                        .emuplat
                        .get_backed_adjust_gpa_range
                        .flatten(),
                )
                .context("failed to initialize GetBackedAdjustGpaRange emuplat")?;

                let adjust_gpa_range = Arc::new(Mutex::new(adjust_gpa_range));

                emuplat_adjust_gpa_range = Some(adjust_gpa_range.clone());

                Box::new(ArcMutexGetBackedAdjustGpaRange(adjust_gpa_range))
            },
        })
    } else {
        emuplat_adjust_gpa_range = None;
        None
    };

    #[cfg(guest_arch = "x86_64")]
    let deps_generic_pic = chipset.with_generic_pic.then_some(dev::GenericPicDeps {});

    #[cfg(not(guest_arch = "x86_64"))]
    let deps_generic_pic = None;

    let deps_generic_isa_dma = chipset
        .with_generic_isa_dma
        .then_some(dev::GenericIsaDmaDeps);
    let deps_generic_pit = chipset.with_generic_pit.then_some(dev::GenericPitDeps {});
    let deps_piix4_pci_isa_bridge =
        chipset
            .with_piix4_pci_isa_bridge
            .then(|| dev::Piix4PciIsaBridgeDeps {
                attached_to: pci_bus_id_piix4.clone(),
            });
    let deps_piix4_pci_usb_uhci_stub =
        chipset
            .with_piix4_pci_usb_uhci_stub
            .then(|| dev::Piix4PciUsbUhciStubDeps {
                attached_to: pci_bus_id_piix4.clone(),
            });
    let deps_piix4_power_management =
        chipset
            .with_piix4_power_management
            .then(|| dev::Piix4PowerManagementDeps {
                attached_to: pci_bus_id_piix4.clone(),
                pm_timer_assist: Some(Box::new(UnderhillPmTimerAssist {
                    partition: Arc::downgrade(&partition),
                })),
            });

    let deps_winbond_super_io_and_floppy_stub = chipset
        .with_winbond_super_io_and_floppy_stub
        .then_some(dev::WinbondSuperIoAndFloppyStubDeps);

    #[cfg(not(guest_arch = "x86_64"))]
    let deps_piix4_cmos_rtc = None;

    #[cfg(guest_arch = "x86_64")]
    let deps_piix4_cmos_rtc = chipset.with_piix4_cmos_rtc.then(|| dev::Piix4CmosRtcDeps {
        time_source: Box::new(rtc_time_source.new_linked_clock()),
        initial_cmos: Some(firmware_pcat::default_cmos_values(&mem_layout)),
        enlightened_interrupts: true, // As advertised by the PCAT BIOS.
    });

    let deps_hyperv_ide = if chipset.with_hyperv_ide {
        let [primary_channel_drives, secondary_channel_drives] = ide_drives;
        Some(dev::HyperVIdeDeps {
            attached_to: pci_bus_id_piix4.clone(),
            primary_channel_drives,
            secondary_channel_drives,
        })
    } else {
        // Ensured above.
        assert!(ide_drives.iter().flatten().all(|d| d.is_none()));
        None
    };

    let deps_underhill_vga_proxy =
        chipset
            .with_underhill_vga_proxy
            .then(|| dev::UnderhillVgaProxyDeps {
                attached_to: pci_bus_id_piix4,
                pci_cfg_proxy: Arc::new(crate::emuplat::vga_proxy::GetProxyVgaPciCfgAccess(
                    get_client.clone(),
                )),
                register_host_io_fastpath: Box::new(UhRegisterHostIoFastPath(partition.clone())),
            });

    let deps_hyperv_guest_watchdog = if chipset.with_hyperv_guest_watchdog {
        Some(dev::HyperVGuestWatchdogDeps {
            port_base: WDAT_PORT,
            watchdog_platform: {
                let store = if let Some(vmgs_client) = vmgs_client.as_ref() {
                    vmgs_client
                        .as_non_volatile_store(vmgs::FileId::GUEST_WATCHDOG, false)
                        .context("failed to instantiate guest watchdog store")?
                } else {
                    EphemeralNonVolatileStore::new_boxed()
                };

                let watchdog_callback = WatchdogTimeoutReset {
                    halt_vps: halt_vps.clone(),
                    watchdog_send: None, // This is not the UEFI watchdog, so no need to send
                                         // watchdog notifications.
                };

                let mut underhill_watchdog_platform =
                    UnderhillWatchdogPlatform::new(store, get_client.clone()).await?;
                underhill_watchdog_platform.add_callback(Box::new(watchdog_callback));

                Box::new(underhill_watchdog_platform)
            },
        })
    } else {
        None
    };

    let deps_generic_psp = { chipset.with_generic_psp.then_some(dev::GenericPspDeps {}) };

    let deps_generic_cmos_rtc = chipset
        .with_generic_cmos_rtc
        .then(|| dev::GenericCmosRtcDeps {
            irq: 8,
            time_source: Box::new(rtc_time_source.new_linked_clock()),
            century_reg_idx: 0x32,
            initial_cmos: None,
        });

    if dps.general.tpm_enabled {
        let no_persistent_secrets =
            vmgs_client.is_none() || dps.general.suppress_attestation.unwrap_or(false);
        let (ppi_store, nvram_store) = if no_persistent_secrets {
            (
                EphemeralNonVolatileStoreHandle.into_resource(),
                EphemeralNonVolatileStoreHandle.into_resource(),
            )
        } else {
            (
                VmgsFileHandle::new(vmgs::FileId::TPM_PPI, true).into_resource(),
                VmgsFileHandle::new(vmgs::FileId::TPM_NVRAM, true).into_resource(),
            )
        };

        // Map the isolation type to the attestation type with Mesh derive so it can be used in resources
        let attestation_type = match isolation {
            virt::IsolationType::Snp => AttestationType::Snp,
            virt::IsolationType::Tdx => AttestationType::Tdx,
            virt::IsolationType::Vbs => AttestationType::Vbs,
            virt::IsolationType::None => AttestationType::Host,
        };

        let ak_cert_type = {
            let request_ak_cert = GetTpmRequestAkCertHelperHandle::new(
                attestation_type,
                attestation_vm_config,
                platform_attestation_data.agent_data,
            )
            .into_resource();

            match attestation_type {
                AttestationType::Snp | AttestationType::Tdx => {
                    TpmAkCertTypeResource::HwAttested(request_ak_cert)
                }
                AttestationType::Vbs => TpmAkCertTypeResource::SwAttested(request_ak_cert),
                AttestationType::Host => TpmAkCertTypeResource::Trusted(
                    request_ak_cert,
                    dps.general
                        .management_vtl_features
                        .control_ak_cert_provisioning()
                        .then(|| {
                            dps.general
                                .management_vtl_features
                                .attempt_ak_cert_callback()
                        }),
                ),
            }
        };

        let register_layout = if cfg!(guest_arch = "x86_64") {
            TpmRegisterLayout::IoPort
        } else {
            TpmRegisterLayout::Mmio
        };

        chipset_devices.push(ChipsetDeviceHandle {
            name: "tpm".to_owned(),
            resource: RemoteChipsetDeviceHandle {
                device: TpmDeviceHandle {
                    ppi_store,
                    nvram_store,
                    refresh_tpm_seeds: platform_attestation_data
                        .host_attestation_settings
                        .refresh_tpm_seeds,
                    ak_cert_type,
                    register_layout,
                    guest_secret_key: platform_attestation_data.guest_secret_key,
                    logger: Some(GetTpmLoggerHandle.into_resource()),
                    is_confidential_vm: isolation.is_isolated(),
                    bios_guid: dps.general.bios_guid,
                }
                .into_resource(),
                worker_host: control_send
                    .call_failable(ControlRequest::MakeWorker, "tpm".into())
                    .await?,
            }
            .into_resource(),
        });
    };

    let deps_hyperv_power_management =
        chipset
            .with_hyperv_power_management
            .then(|| dev::HyperVPowerManagementDeps {
                acpi_irq: SYSTEM_IRQ_ACPI,
                pio_base: PM_BASE,
                pm_timer_assist: Some(Box::new(UnderhillPmTimerAssist {
                    partition: Arc::downgrade(&partition),
                })),
            });

    let devices = BaseChipsetDevices {
        deps_generic_cmos_rtc,
        deps_generic_ioapic,
        deps_generic_psp,
        deps_hyperv_firmware_uefi,
        deps_hyperv_guest_watchdog,
        deps_hyperv_power_management,
        deps_generic_isa_dma,
        deps_generic_isa_floppy: None,
        deps_generic_pci_bus: None,
        deps_generic_pic,
        deps_generic_pit,
        deps_hyperv_firmware_pcat,
        deps_hyperv_framebuffer: None,
        deps_hyperv_ide,
        deps_hyperv_vga: None,
        deps_i440bx_host_pci_bridge,
        deps_piix4_cmos_rtc,
        deps_piix4_pci_bus,
        deps_piix4_pci_isa_bridge,
        deps_piix4_pci_usb_uhci_stub,
        deps_piix4_power_management,
        deps_underhill_vga_proxy,
        deps_winbond_super_io_and_floppy_stub,
        deps_winbond_super_io_and_floppy_full: None,
    };

    let fallback_mmio_device = if use_mmio_hypercalls {
        let mshv_hvcall =
            hcl::ioctl::MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        mshv_hvcall.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallMemoryMappedIoRead,
            hvdef::HypercallCode::HvCallMemoryMappedIoWrite,
        ]);

        // If VTOM is present (CVM scenario), accesses to physical device and PCI config space may
        // occur below or above vTOM, but only within MMIO regions. Forward both to the host.
        let vtom = vtom.unwrap_or(0);
        let untrusted_mmio_ranges =
            if cfg!(guest_arch = "aarch64") && vtom == 0 {
                // By default on aarch64 send all MMIO accesses to the host.
                None
            } else {
                let mut untrusted_mmio_ranges: Vec<_> = mem_layout.mmio().to_vec();
                if vtom > 0 {
                    untrusted_mmio_ranges.extend(mem_layout.mmio().iter().map(|range| {
                        MemoryRange::new((range.start() + vtom)..(range.end() + vtom))
                    }));
                }
                Some(untrusted_mmio_ranges)
            };

        Some(Arc::new(CloseableMutex::new(FallbackMmioDevice {
            mmio_ranges: untrusted_mmio_ranges,
            mshv_hvcall,
        })) as _)
    } else {
        None
    };

    let BaseChipsetBuilderOutput {
        mut chipset_builder,
        device_interfaces: _,
    } = BaseChipsetBuilder::new(
        BaseChipsetFoundation {
            is_restoring,
            untrusted_dma_memory: device_memory.clone(),
            trusted_vtl0_dma_memory: gm.vtl0().clone(),
            vmtime: &vmtime_source,
            vmtime_unit: vmtime.handle(),
            doorbell_registration: None,
            power_event_handler: halt_vps.clone(),
            debug_event_handler: halt_vps.clone(),
        },
        devices,
    )
    .with_expected_manifest(chipset)
    .with_device_handles(chipset_devices)
    .with_trace_unknown_mmio(!use_mmio_hypercalls)
    .with_fallback_mmio_device(fallback_mmio_device)
    .build(&driver_source, &state_units, &resolver)
    .instrument(tracing::info_span!("base_chipset_build", CVM_ALLOWED))
    .await
    .context("failed to create devices")?;

    // Add the x86 BSP's LINTs for the PIC to use.
    #[cfg(guest_arch = "x86_64")]
    chipset_builder.add_external_line_target(
        BSP_LINT_LINE_SET,
        0..=1,
        0,
        "bsp",
        Arc::new(vmm_core::emuplat::apic::ApicLintLineTarget::new(
            partition.clone(),
            Vtl::Vtl0,
        )),
    );

    // Add the GIC.
    #[cfg(guest_arch = "aarch64")]
    chipset_builder.add_external_line_target(
        chipset_device_resources::IRQ_LINE_SET,
        0..=vmm_core::emuplat::gic::SPI_RANGE.end() - vmm_core::emuplat::gic::SPI_RANGE.start(),
        *vmm_core::emuplat::gic::SPI_RANGE.start(),
        "gic",
        Arc::new(vmm_core::emuplat::gic::GicInterruptTarget::new(
            partition.clone().control_gic(Vtl::Vtl0),
        )),
    );

    // TODO: Enable for isolated VMs.
    // Intercept shutdown device from VTL0 when the vmbus relay is active.
    // N.B. Skip enabling if restoring from a previous version that did not use
    //      a shutdown relay, otherwise would have to rescind the host shutdown
    //      channel from the lower-VTL guest before taking control.
    let intercept_shutdown_ic = !hardware_isolated
        && (!is_restoring || servicing_state.overlay_shutdown_device.unwrap_or(false));
    let mut intercepted_shutdown_ic = None;

    let mut vmbus_server = None;
    let mut vmbus_client = None;
    let mut host_vmbus_relay = None;
    let mut vmbus_filter = None;
    let mut vpci_relay = None;

    // VMBus
    if with_vmbus {
        let (server_relay, hvsock_notify, relay_channels) = if with_vmbus_relay {
            let channel = vmbus_server::VmbusRelayChannel::new();
            let hvsock = vmbus_server::HvsockRelayChannel::new();
            (
                Some(channel.server_half),
                Some(hvsock.server_half),
                Some((channel.relay_half, hvsock.relay_half)),
            )
        } else {
            (None, None, None)
        };

        // If the MNF option value is provided, then use that.
        // If the option value is not provided and networking is configured through
        // underhill, enable MNF.
        let enable_mnf = env_cfg
            .vmbus_enable_mnf
            .unwrap_or(!controllers.mana.is_empty());
        tracing::info!(CVM_ALLOWED, enable_mnf, "Underhill MNF enabled?");

        let max_version = env_cfg
            .vmbus_max_version
            .map(vmbus_core::MaxVersionInfo::new)
            .or_else(|| {
                // For compatibility with rollback, any additional features are currently disabled,
                // except for isolated guests which do not support servicing.
                (!hardware_isolated).then_some(vmbus_core::MaxVersionInfo {
                    version: vmbus_core::protocol::Version::Copper as u32,
                    feature_flags: vmbus_core::protocol::FeatureFlags::new()
                        .with_guest_specified_signal_parameters(true)
                        .with_channel_interrupt_redirection(true)
                        .with_modify_connection(true),
                })
            });

        // Delay the max version if the requested version is older than what the UEFI firmware
        // supports.
        let delay_max_version = if let Some(max_version) = max_version {
            firmware_type == FirmwareType::Uefi
                && max_version.version < vmbus_core::protocol::Version::Win10 as u32
        } else {
            false
        };

        // N.B. VmBus uses untrusted memory by default for relay channels, and uses additional
        //      trusted memory only for confidential channels offered by Underhill itself.
        let vmbus =
            VmbusServer::builder(driver_source.simple(), synic.clone(), device_memory.clone())
                .private_gm(gm.cvm_memory().map(|x| &x.private_vtl0_memory).cloned())
                .hvsock_notify(hvsock_notify)
                .server_relay(server_relay)
                .max_version(max_version)
                .delay_max_version(delay_max_version)
                .enable_mnf(enable_mnf)
                .force_confidential_external_memory(
                    env_cfg.vmbus_force_confidential_external_memory,
                )
                .channel_unstick_delay(env_cfg.vmbus_channel_unstick_delay)
                // For saved-state compat with release/2411.
                .send_messages_while_stopped(true)
                .build()
                .context("failed to create vmbus server")?;

        let vmbus = VmbusServerHandle::new(&tp, state_units.add("vmbus"), vmbus)?;
        if let Some((relay_channel, hvsock_relay)) = relay_channels {
            let relay_driver = tp.driver(0);
            let builder = vmbus_client_hcl::vmbus_client_builder(relay_driver)
                .context("failed to create synic client and message source")?;

            let mut client = builder.build(tp);
            let connection = if let Some(state) = servicing_state.vmbus_client.flatten() {
                client.restore(state).await?
            } else {
                None
            };
            client.start();
            let connection = if let Some(c) = connection {
                c
            } else {
                // Unique ID used so that the host knows which client this is,
                // for diagnosing failures.
                const OPENHCL_CLIENT_ID: Guid = guid::guid!("ceb1cd55-6a3b-41c5-9473-4dd30624c3d8");
                client
                    .connect(0, None, OPENHCL_CLIENT_ID)
                    .await
                    .context("failed to connect to vmbus")?
            };

            let mut filter = vmbus_client::filter::ClientFilterBuilder::new();

            let mut vpci_filter = vmbus_client::filter::FilterDefinition::new("vpci")
                .by_interface(guid::guid!("44C4F61D-4444-4400-9D52-802E27EDE19F"));

            if enable_vpci_relay {
                filter.add(&mut vpci_filter);
            }

            let mut relay_filter = vmbus_client::filter::FilterDefinition::new("relay").rest();
            filter.add(&mut relay_filter);
            vmbus_filter = Some(filter.build(tp, connection));

            let connection = relay_filter.take();

            if enable_vpci_relay {
                use vpci_relay::*;

                let mut relay = VpciRelay::new(
                    driver_source.clone(),
                    vpci_filter.take(),
                    vmbus.control().clone(),
                    dma_manager.new_client(DmaClientParameters {
                        device_name: "vpci-relay".into(),
                        lower_vtl_policy: LowerVtlPermissionPolicy::Vtl0,
                        allocation_visibility: if hardware_isolated {
                            AllocationVisibility::Shared
                        } else {
                            AllocationVisibility::Private
                        },
                        persistent_allocations: false,
                    })?,
                    vpci_relay_mmio,
                    if use_mmio_hypercalls {
                        Box::new(
                            linux_mmio::HypercallMmio::new()
                                .context("failed to create hypercall mmio accessor")?,
                        )
                    } else {
                        Box::new(
                            linux_mmio::DirectMmio::new()
                                .context("failed to create direct mmio accessor")?,
                        )
                    },
                    vtom,
                );

                // Allow NVMe devices.
                relay.add_allowed_device(AllowedDevice {
                    vendor_id: None,
                    device_id: None,
                    revision_id: None,
                    prog_if: Some(
                        ProgrammingInterface::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY_NVME,
                    ),
                    sub_class: Some(Subclass::MASS_STORAGE_CONTROLLER_NON_VOLATILE_MEMORY),
                    base_class: Some(ClassCode::MASS_STORAGE_CONTROLLER),
                    sub_vendor_id: None,
                    sub_system_id: None,
                });

                // Allow MANA devices.
                relay.add_allowed_device(AllowedDevice {
                    vendor_id: Some(gdma_defs::VENDOR_ID),
                    device_id: Some(gdma_defs::DEVICE_ID),
                    revision_id: None,
                    prog_if: Some(ProgrammingInterface::NETWORK_CONTROLLER_ETHERNET_GDMA),
                    sub_class: Some(Subclass::NETWORK_CONTROLLER_ETHERNET),
                    base_class: Some(ClassCode::NETWORK_CONTROLLER),
                    sub_vendor_id: None,
                    sub_system_id: None,
                });

                // Allow Azi HSM devices.
                relay.add_allowed_device(AllowedDevice {
                    vendor_id: Some(AZIHSM_VPCI_VENDOR_ID), // Microsoft vendor ID
                    device_id: Some(AZIHSM_VPCI_DEVICE_ID), // Azi HSM device ID
                    revision_id: None,
                    prog_if: Some(ProgrammingInterface::NONE),
                    sub_class: Some(Subclass::NONE),
                    base_class: Some(ClassCode::ENCRYPTION_CONTROLLER),
                    sub_vendor_id: None,
                    sub_system_id: None,
                });

                vpci_relay = Some(relay);
            }

            let mut intercept_list = Vec::new();
            if intercept_shutdown_ic {
                let (send, recv) = mesh::channel();
                intercept_list.push((hyperv_ic_guest::shutdown::INSTANCE_ID, send));
                intercepted_shutdown_ic = Some(recv);
            }

            let vmbus_relay = vmbus_relay::HostVmbusTransport::new(
                relay_driver.clone(),
                Arc::clone(vmbus.control()),
                relay_channel,
                hvsock_relay,
                client.access().clone(),
                connection,
                intercept_list,
            )
            .await
            .context("failed to create host vmbus transport")?;

            host_vmbus_relay = Some(VmbusRelayHandle::new(
                &tp,
                state_units
                    .add("vmbus_relay")
                    .depends_on(vmbus.unit_handle()),
                vmbus_relay,
            )?);

            vmbus_client = Some(client);
        }

        vmbus_server = Some(vmbus);
    }

    let mut vmbus_device_handles = controllers.vmbus_devices;

    // Storage
    let mut ide_accel_devices = Vec::new();
    {
        let _span = tracing::info_span!("scsi_controller_map", CVM_ALLOWED).entered();

        for (path, scsi_disk) in storvsp_ide_disks {
            let io_queue_depth = ide_io_queue_depth.unwrap_or(default_io_queue_depth);
            ide_accel_devices.push(
                offer_channel_unit(
                    &tp,
                    &state_units,
                    vmbus_server
                        .as_ref()
                        .context("ide requires vmbus redirection to be configured")?,
                    storvsp::StorageDevice::build_ide(
                        &driver_source,
                        path.channel,
                        path.drive,
                        scsi_disk,
                        io_queue_depth,
                    ),
                )
                .await?,
            );
        }
    }

    // VPCI
    //
    // Use a cfg block instead of an if(cfg!) because the compiler does a bad
    // job eliminating the dead code.
    #[cfg(feature = "vpci")]
    {
        use virt::Hv1;
        use vmcore::vpci_msi::VpciInterruptMapper;

        for crate::dispatch::vtl2_settings_worker::UhVpciDeviceConfig {
            instance_id,
            resource,
        } in controllers.vpci_devices
        {
            let vmbus = vmbus_server
                .as_ref()
                .context("vpci devices require vmbus redirection to be enabled")?;

            vmm_core::device_builder::build_vpci_device(
                &driver_source,
                &resolver,
                device_memory,
                vmbus.control(),
                instance_id,
                resource,
                &mut chipset_builder,
                None,
                None,
                |device_id| {
                    let device = partition
                        .new_virtual_device()
                        .context("vpci is not supported by this hypervisor")?
                        .build(Vtl::Vtl0, device_id)?;
                    let device = Arc::new(device);
                    Ok((device.clone(), VpciInterruptMapper::new(device)))
                },
                vtom,
            )
            .await?;
        }
    }
    #[cfg(not(feature = "vpci"))]
    if !controllers.vpci_devices.is_empty() {
        anyhow::bail!("built without vpci support");
    }

    // Networking
    let mut uh_network_settings = UhVmNetworkSettings {
        nics: Vec::new(),
        vf_managers: HashMap::new(),
        get_client: get_client.clone(),
        vp_count: vps.len(),
        dma_mode: if hide_isolation {
            net_mana::GuestDmaMode::BounceBuffer
        } else {
            net_mana::GuestDmaMode::DirectDma
        },
    };
    let mut netvsp_state = Vec::with_capacity(controllers.mana.len());
    if !controllers.mana.is_empty() {
        let _span = tracing::info_span!("network_settings", CVM_ALLOWED).entered();
        for nic_config in controllers.mana.into_iter() {
            let nic_servicing_state = if let Some(ref state) = servicing_state.mana_state {
                state.iter().find(|s| s.pci_id == nic_config.pci_id)
            } else {
                None
            };

            let save_state = uh_network_settings
                .add_network(
                    nic_config.instance_id,
                    nic_config.subordinate_instance_id,
                    nic_config.max_sub_channels,
                    tp,
                    &uevent_listener,
                    &servicing_state.emuplat.netvsp_state,
                    partition.clone(),
                    &state_units,
                    &vmbus_server,
                    dma_manager.client_spawner(),
                    isolation.is_isolated(),
                    env_cfg.mana_keep_alive.clone(),
                    nic_servicing_state,
                )
                .await?;

            netvsp_state.push(save_state);
        }
    }
    let network_settings: Option<Box<dyn LoadedVmNetworkSettings>> =
        Some(Box::new(uh_network_settings));

    if let Some(framebuffer) = remote_console_cfg.framebuffer {
        resolver.add_resolver(FramebufferRemoteControl {
            get: get_client.clone(),
            format_send: framebuffer.format_send(),
        });

        vmbus_device_handles.push(
            uidevices_resources::SynthVideoHandle {
                framebuffer: video_core::SharedFramebufferHandle.into_resource(),
            }
            .into_resource(),
        );
        vmbus_device_handles.push(
            uidevices_resources::SynthKeyboardHandle {
                source: MultiplexedInputHandle { elevation: 1 }.into_resource(),
            }
            .into_resource(),
        );
        vmbus_device_handles.push(
            uidevices_resources::SynthMouseHandle {
                source: MultiplexedInputHandle { elevation: 1 }.into_resource(),
            }
            .into_resource(),
        );
    }

    let shutdown_relay = if let Some(recv) = intercepted_shutdown_ic {
        let mut shutdown_guest = ShutdownGuestIc::new();
        let recv_host_shutdown = shutdown_guest.get_shutdown_notifier();
        let (send_guest_shutdown, recv_guest_shutdown) = mesh::channel();

        // Expose a different shutdown device to the VTL0 guest.
        vmbus_device_handles.push(
            hyperv_ic_resources::shutdown::ShutdownIcHandle {
                recv: recv_guest_shutdown,
            }
            .into_resource(),
        );

        let shutdown_guest = SimpleVmbusClientDeviceWrapper::new(
            driver_source.simple(),
            dma_manager
                .new_client(DmaClientParameters {
                    device_name: "shutdown-relay".into(),
                    lower_vtl_policy: LowerVtlPermissionPolicy::Vtl0,
                    allocation_visibility: AllocationVisibility::Private,
                    persistent_allocations: false,
                })
                .context("shutdown relay dma client")?,
            shutdown_guest,
        )?;
        shutdown_guest.detach(driver_source.simple(), recv)?;
        Some((recv_host_shutdown, send_guest_shutdown))
    } else {
        None
    };

    // Add vmbus devices.
    let mut vmbus_devices = Vec::new();
    for resource in vmbus_device_handles {
        let vmbus = vmbus_server.as_ref().with_context(|| {
            format!(
                "device '{}' requires vmbus redirection to be configured",
                resource.id()
            )
        })?;

        vmbus_devices.push(
            offer_vmbus_device_handle_unit(
                &driver_source,
                &state_units,
                vmbus,
                &resolver,
                resource,
            )
            .await?,
        );
    }

    let control_send = Arc::new(Mutex::new(Some(control_send)));

    let (chipset, devices) = chipset_builder.build()?;
    let (fatal_error_send, fatal_error_recv) = mesh::channel();
    let control_send_clone = control_send.clone();
    let fatal_error_policy = if env_cfg.halt_on_guest_halt {
        vmm_core::vmotherboard_adapter::FatalErrorPolicy::DebugBreak(fatal_error_send)
    } else {
        vmm_core::vmotherboard_adapter::FatalErrorPolicy::Panic(Arc::new(move || {
            // TODO: Make this closure async?
            block_on(wait_for_flush_logs(&control_send_clone));
        }))
    };
    let chipset = vmm_core::vmotherboard_adapter::ChipsetPlusSynic::new(
        synic.clone(),
        chipset,
        fatal_error_policy,
    );

    if let Some(vpci_relay) = &mut vpci_relay {
        // Relay the initial set of VPCI devices.
        vpci_relay
            .process(&devices, &mut state_units)
            .await
            .context("failed to relay initial vpci channels")?;
    }
    let (halt_notify_send, halt_notify_recv) = mesh::channel();
    let halt_task = tp.spawn(
        "halt",
        halt_task(
            halt_notify_recv,
            fatal_error_recv,
            control_send.clone(),
            get_client.clone(),
            env_cfg.halt_on_guest_halt,
        ),
    );

    let (mut partition_unit, vp_runners) = PartitionUnit::new(
        tp,
        state_units
            .add("partition")
            .depends_on(devices.chipset_unit())
            .depends_on(vmtime.handle()),
        WrappedPartition(partition.clone()),
        PartitionUnitParams {
            processor_topology: &processor_topology,
            halt_vps,
            halt_request_recv,
            client_notify_send: halt_notify_send,
            vtl_guest_memory: [Some(gm.vtl0()), gm.vtl1(), None],
            debugger_rpc,
        },
    )
    .context("failed to create partition unit")?;

    dma_manager
        .validate_restore()
        .context("failed to validate restore for dma manager")?;

    // Start the VP tasks on the thread pool.
    crate::vp::spawn_vps(tp, vps, vp_runners, &chipset, isolation)
        .await
        .context("failed to spawn vps")?;

    // Load the firmware.
    if let Some(vtl0_info) = measured_vtl0_info {
        load_firmware(
            gm.vtl0(),
            &mem_layout,
            &processor_topology,
            &vtl0_memory_map,
            &mut partition_unit,
            &partition,
            env_cfg.cmdline_append.as_deref(),
            vtl0_info,
            &runtime_params,
            load_kind,
            &dps,
            isolation.is_isolated(),
        )
        .instrument(tracing::info_span!("load_firmware", CVM_ALLOWED))
        .await?;
    }

    // Construct a LoadedVm struct directly, and call the common run loop.
    let loaded_vm = LoadedVm {
        partition_unit,
        memory: gm,
        firmware_type,
        isolation,
        chipset_devices: devices,
        _vmtime: vmtime,
        _halt_task: halt_task,
        state_units,
        last_state_unit_stop: (servicing_state.vm_stop_reference_time).map(ReferenceTime::new),
        partition,
        uevent_listener,
        resolver,
        nvme_manager,
        emuplat_servicing: EmuplatServicing {
            get_backed_adjust_gpa_range: emuplat_adjust_gpa_range,
            rtc_local_clock: rtc_time_source.0,
            netvsp_state,
        },
        device_interfaces: Some(controllers.device_interfaces),
        vmbus_client,
        vmbus_filter,
        vpci_relay,
        vtl0_memory_map,

        vmbus_server,
        host_vmbus_relay,
        _vmbus_devices: vmbus_devices,
        _ide_accel_devices: ide_accel_devices,
        network_settings,
        shutdown_relay,

        vmgs,

        get_client: get_client.clone(),
        device_platform_settings: dps,
        runtime_params,

        _input_distributor: input_distributor,

        crash_notification_recv,
        control_send,

        _periodic_telemetry_task: periodic_telemetry_task,
        nvme_keep_alive: env_cfg.nvme_keep_alive,
        mana_keep_alive: env_cfg.mana_keep_alive,
        test_configuration: env_cfg.test_configuration,
        dma_manager,
        config_timeout_in_seconds: env_cfg.config_timeout_in_seconds,
    };

    Ok(loaded_vm)
}

fn validate_isolated_configuration(dps: &DevicePlatformSettings) -> Result<(), anyhow::Error> {
    let General {
        // Attested to
        secure_boot_enabled,
        tpm_enabled: _,
        com1_enabled: _,
        com1_vmbus_redirector: _,
        com2_enabled: _,
        com2_vmbus_redirector: _,
        suppress_attestation: _,
        bios_guid: _,
        vpci_boot_enabled: _,

        // Validated below
        battery_enabled,
        processor_idle_enabled,
        firmware_debugging_enabled,
        hibernation_enabled,
        legacy_memory_map,
        measure_additional_pcrs,
        disable_sha384_pcr,
        is_servicing_scenario,
        firmware_mode_is_pcat,
        psp_enabled,
        default_boot_always_attempt,

        // Minimum level enforced by UEFI loader
        memory_protection_mode: _,

        // Both options supported
        vmbus_redirection_enabled: _,
        always_relay_host_mmio: _,
        imc_enabled: _,

        // PXE not supported today
        pxe_ip_v6: _,
        media_present_enabled_by_default: _,

        // Does not affect isolation
        secure_boot_template: _,
        console_mode: _,
        com1_debugger_mode: _,
        com2_debugger_mode: _,
        generation_id: _,
        pause_after_boot_failure: _,
        disable_frontpage: _,
        num_lock_enabled: _,
        pcat_boot_device_order: _,
        vpci_instance_filter: _,
        nvdimm_count: _,
        watchdog_enabled: _,
        vtl2_settings: _,
        cxl_memory_enabled: _,

        // TODO: decide whether these need to be validated here
        efi_diagnostics_log_level: _,
        guest_state_encryption_policy: _,
        guest_state_lifetime: _,
        management_vtl_features: _,
        hv_sint_enabled: _,
    } = &dps.general;

    if *hibernation_enabled {
        anyhow::bail!("hibernation is not supported");
    }
    if *processor_idle_enabled {
        anyhow::bail!("processor idle is not supported");
    }
    if *secure_boot_enabled && *firmware_debugging_enabled {
        anyhow::bail!("secure boot and firmware debugging are mutually exclusive");
    }
    if *battery_enabled {
        anyhow::bail!("battery is not supported");
    }
    if *legacy_memory_map {
        anyhow::bail!("legacy memory map is not supported");
    }
    if !*measure_additional_pcrs {
        anyhow::bail!("additional PCRs must be measured");
    }
    if *disable_sha384_pcr {
        anyhow::bail!("SHA-384 PCR must not be disabled");
    }
    if *is_servicing_scenario {
        anyhow::bail!("servicing is not yet supported");
    }
    if *firmware_mode_is_pcat {
        anyhow::bail!("firmware mode must not be PCAT");
    }
    if *psp_enabled {
        anyhow::bail!("PSP is not yet supported");
    }
    if *default_boot_always_attempt {
        anyhow::bail!("default_boot_always_attempt is not supported");
    }

    Ok(())
}

/// Builds VP register state to send over GET.
fn build_vp_state<T, const N: usize>(registers: Option<&T>) -> Vec<RegisterState>
where
    T: HvRegisterState<HvArchRegisterName, N>,
{
    if underhill_confidentiality::confidential_filtering_enabled() {
        return Vec::new();
    }

    let mut reg_state = vec![];

    if let Some(registers) = registers {
        let names = registers.names();
        let mut values = [HvRegisterValue::new_zeroed(); N];
        registers.get_values(values.iter_mut());

        let it = names.iter().zip(values.iter());

        reg_state = it
            .map(|(name, value)| RegisterState {
                name: name.0,
                value: value.as_u128().to_ne_bytes(),
            })
            .collect();
    }

    reg_state
}

/// Waits for halt notifications and handles them by forwarding them to the
/// host (when appropriate).
async fn halt_task(
    mut halt_notify_recv: mesh::Receiver<HaltReason>,
    mut _fatal_error_recv: mesh::Receiver<Box<dyn std::error::Error + Send + Sync>>,
    control_send: Arc<Mutex<Option<mesh::Sender<ControlRequest>>>>,
    get_client: GuestEmulationTransportClient,
    halt_on_guest_halt: bool,
) {
    #[derive(Debug)]
    enum HaltRequest {
        PowerOff,
        Reset,
        Hibernate,
        TripleFault { vp: u32, regs: Vec<RegisterState> },
    }

    while let Ok(reason) = halt_notify_recv.recv().await {
        let halt_request = match reason {
            HaltReason::PowerOff => HaltRequest::PowerOff,
            HaltReason::Reset => HaltRequest::Reset,
            HaltReason::Hibernate => HaltRequest::Hibernate,
            HaltReason::TripleFault { vp, registers } => {
                tracing::info!(CVM_ALLOWED, vp, "triple fault");
                let reg_state = build_vp_state(registers.as_deref());
                HaltRequest::TripleFault {
                    vp,
                    regs: reg_state,
                }
            }
            // Debug halts require no further processing, loop back around.
            HaltReason::DebugBreak { vp } => {
                tracing::info!(CVM_ALLOWED, vp, "debug break");
                continue;
            }
            HaltReason::SingleStep { vp } => {
                tracing::info!(CVM_ALLOWED, vp, "single step");
                continue;
            }
            HaltReason::HwBreakpoint { vp, .. } => {
                tracing::info!(CVM_ALLOWED, vp, "hardware breakpoint");
                continue;
            }
        };

        if halt_on_guest_halt {
            // For guest requested halts, log the error and do not forward to the host.
            tracing::info!(CVM_ALLOWED, ?halt_request, "guest halted");
        } else {
            // All real halts require flushing logs to the host. Wait up to 5 seconds.
            wait_for_flush_logs(&control_send).await;

            // Now we can notify the host about the halt.
            match halt_request {
                HaltRequest::PowerOff => get_client.send_power_off(),
                HaltRequest::Reset => get_client.send_reset(),
                HaltRequest::Hibernate => get_client.send_hibernate(),
                HaltRequest::TripleFault { vp, regs } => {
                    get_client.triple_fault(vp, TripleFaultType::UNRECOVERABLE_EXCEPTION, regs)
                }
            }
        }
    }
}

async fn wait_for_flush_logs(control_send: &Arc<Mutex<Option<mesh::Sender<ControlRequest>>>>) {
    let ctx = CancelContext::new().with_timeout(Duration::from_secs(5));
    let call = control_send
        .lock()
        .as_ref()
        .map(|send| send.call(ControlRequest::FlushLogs, ctx));
    if let Some(call) = call {
        call.await.ok();
    }
}

async fn load_firmware(
    gm: &GuestMemory,
    mem_layout: &MemoryLayout,
    processor_topology: &ProcessorTopology,
    vtl0_memory_map: &[(MemoryRangeWithNode, MemoryMapEntryType)],
    partition_unit: &mut PartitionUnit,
    partition: &UhPartition,
    cmdline_append: Option<&str>,
    vtl0_info: MeasuredVtl0Info,
    runtime_params: &RuntimeParameters,
    load_kind: LoadKind,
    dps: &DevicePlatformSettings,
    isolated: bool,
) -> Result<(), anyhow::Error> {
    let cmdline_append = match cmdline_append {
        Some(cmdline) => CString::new(cmdline.as_bytes()).context("bad command line")?,
        None => CString::default(),
    };
    let loader_config = crate::loader::Config { cmdline_append };
    let caps = partition.caps();
    let vtl0_vp_context = crate::loader::load(
        gm,
        mem_layout,
        processor_topology,
        vtl0_memory_map,
        runtime_params,
        load_kind,
        vtl0_info,
        dps,
        loader_config,
        caps,
        isolated,
    )
    .context("failed to load firmware")?;

    #[cfg(guest_arch = "x86_64")]
    let registers = {
        let crate::loader::VpContext::Vbs(mut registers) = vtl0_vp_context;
        registers.extend(
            loader::common::compute_variable_mtrrs(
                mem_layout,
                partition.caps().physical_address_width,
            )
            .context("Failed to compute variable mtrrs")?,
        );
        registers
    };
    #[cfg(guest_arch = "aarch64")]
    let crate::loader::VpContext::Vbs(registers) = vtl0_vp_context;

    let registers = initial_regs(&registers, caps, &processor_topology.vp_arch(VpIndex::BSP));
    partition_unit
        .set_initial_regs(Vtl::Vtl0, registers)
        .instrument(tracing::info_span!("set_initial_regs", CVM_ALLOWED))
        .await
        .context("failed to set initial registers")?;

    Ok(())
}

pub struct UnderhillPmTimerAssist {
    pub partition: std::sync::Weak<UhPartition>,
}

impl chipset::pm::PmTimerAssist for UnderhillPmTimerAssist {
    fn set(&self, port: Option<u16>) {
        if let Some(partition) = self.partition.upgrade() {
            if let Err(err) = partition.set_pm_timer_assist(port) {
                tracing::warn!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    ?port,
                    "failed to set PM timer assist"
                );
            }
        }
    }
}

// Represents a stub MMIO device that handles unhandled MMIO accesses by
// forwarding them to the host. It needs to implement the ChipsetDevice and
// MmioIntercept traits.
struct FallbackMmioDevice {
    mmio_ranges: Option<Vec<MemoryRange>>,
    mshv_hvcall: hcl::ioctl::MshvHvcall,
}

impl FallbackMmioDevice {
    fn is_allowed(&self, addr: u64, data_len: usize) -> bool {
        self.mmio_ranges.as_ref().is_none_or(|v| {
            v.iter().any(|range| {
                range.contains_addr(addr) && range.contains_addr(addr + data_len as u64 - 1)
            })
        })
    }
}

impl chipset_device::mmio::MmioIntercept for FallbackMmioDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> chipset_device::io::IoResult {
        data.fill(!0);
        if self.is_allowed(addr, data.len()) {
            if let Err(err) = self.mshv_hvcall.mmio_read(addr, data) {
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "failed host MMIO read"
                );
            }
        }

        chipset_device::io::IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> chipset_device::io::IoResult {
        if self.is_allowed(addr, data.len()) {
            if let Err(err) = self.mshv_hvcall.mmio_write(addr, data) {
                tracelimit::error_ratelimited!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "failed host MMIO write"
                );
            }
        }

        chipset_device::io::IoResult::Ok
    }
}

impl ChipsetDevice for FallbackMmioDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn chipset_device::mmio::MmioIntercept> {
        Some(self)
    }
}

#[cfg(guest_arch = "x86_64")]
struct WatchdogTimeoutNmi {
    partition: Arc<UhPartition>,
    watchdog_send: Option<mesh::Sender<()>>,
}

#[cfg(guest_arch = "x86_64")]
#[async_trait::async_trait]
impl WatchdogCallback for WatchdogTimeoutNmi {
    async fn on_timeout(&mut self) {
        crate::livedump::livedump().await;

        // Unlike Hyper-V, we only send the NMI to the BSP.
        self.partition.request_msi(
            Vtl::Vtl0,
            MsiRequest::new_x86(virt::irqcon::DeliveryMode::NMI, 0, false, 0, false),
        );

        if let Some(watchdog_send) = &self.watchdog_send {
            watchdog_send.send(());
        }
    }
}

struct WatchdogTimeoutReset {
    halt_vps: Arc<Halt>,
    watchdog_send: Option<mesh::Sender<()>>,
}

#[async_trait::async_trait]
impl WatchdogCallback for WatchdogTimeoutReset {
    async fn on_timeout(&mut self) {
        crate::livedump::livedump().await;

        self.halt_vps.halt(HaltReason::Reset);

        if let Some(watchdog_send) = &self.watchdog_send {
            watchdog_send.send(());
        }
    }
}

#[derive(MeshPayload, Clone)]
pub struct OpenHclRemoteDynamicResolvers {
    /// GET client
    pub get: GuestEmulationTransportClient,
    /// VMGS client
    pub vmgs: Option<vmgs_broker::VmgsClient>,
}

impl chipset_device_worker::RemoteDynamicResolvers for OpenHclRemoteDynamicResolvers {
    const WORKER_ID_STR: &str = "openhcl_remote_chipset_worker";

    async fn register_remote_dynamic_resolvers(
        self,
        resolver: &mut ResourceResolver,
    ) -> anyhow::Result<()> {
        resolver.add_resolver(self.get);
        if let Some(vmgs) = self.vmgs {
            resolver.add_resolver(vmgs);
        }
        Ok(())
    }
}

mesh_worker::register_workers! {
    chipset_device_worker::worker::RemoteChipsetDeviceWorker<OpenHclRemoteDynamicResolvers>
}
