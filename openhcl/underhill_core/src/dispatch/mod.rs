// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements vm dispatch and vm state management for underhill.

mod pci_shutdown;
pub mod vtl2_settings_worker;

use self::vtl2_settings_worker::DeviceInterfaces;
use crate::ControlRequest;
use crate::emuplat::EmuplatServicing;
use crate::emuplat::netvsp::RuntimeSavedState;
use crate::nvme_manager::manager::NvmeManager;
use crate::options::KeepAliveConfig;
use crate::options::TestScenarioConfig;
use crate::reference_time::ReferenceTime;
use crate::servicing;
use crate::servicing::NvmeSavedState;
use crate::servicing::ServicingState;
use crate::vmbus_relay_unit::VmbusRelayHandle;
use crate::worker::FirmwareType;
use crate::worker::NetworkSettingsError;
use anyhow::Context;
use async_trait::async_trait;
use cvm_tracing::CVM_ALLOWED;
use futures::FutureExt;
use futures::StreamExt;
use futures_concurrency::future::Join;
use get_protocol::SaveGuestVtl2StateFlags;
use guest_emulation_transport::api::GuestSaveRequest;
use guid::Guid;
use hyperv_ic_resources::shutdown::ShutdownParams;
use hyperv_ic_resources::shutdown::ShutdownResult;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use hyperv_ic_resources::shutdown::ShutdownType;
use igvm_defs::MemoryMapEntryType;
use inspect::Inspect;
use mana_driver::save_restore::ManaSavedState;
use mesh::CancelContext;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use mesh_worker::WorkerRpc;
use net_packet_capture::PacketCaptureParams;
use openhcl_dma_manager::DmaClientSpawner;
use openhcl_dma_manager::OpenhclDmaManager;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use socket2::Socket;
use state_unit::SavedStateUnit;
use state_unit::SpawnedUnit;
use state_unit::StateUnits;
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;
use tracing::instrument;
use uevent::UeventListener;
use underhill_threadpool::AffinitizedThreadpool;
use virt::IsolationType;
use virt_mshv_vtl::UhPartition;
use virt_mshv_vtl::VtlCrash;
use vm_resource::ResourceResolver;
use vm_topology::memory::MemoryRangeWithNode;
use vmbus_channel::channel::VmbusDevice;
use vmcore::vmtime::VmTimeKeeper;
use vmm_core::input_distributor::InputDistributor;
use vmm_core::partition_unit::PartitionUnit;
use vmm_core::vmbus_unit::ChannelUnit;
use vmm_core::vmbus_unit::VmbusServerHandle;
use vmotherboard::ChipsetDevices;
use vtl2_settings_worker::Vtl2ConfigNicRpc;
use vtl2_settings_worker::Vtl2SettingsWorker;
use vtl2_settings_worker::handle_vtl2_config_rpc;

#[derive(MeshPayload)]
pub enum UhVmRpc {
    Pause(Rpc<(), bool>),
    Resume(Rpc<(), bool>),
    Save(FailableRpc<(), Vec<u8>>),
    ClearHalt(Rpc<(), bool>), // TODO: remove this, and use DebugRequest::Resume
    PacketCapture(FailableRpc<PacketCaptureParams<Socket>, PacketCaptureParams<Socket>>),
}

#[async_trait]
pub trait LoadedVmNetworkSettings: Inspect {
    /// Callback to prepare for guest hibernation. This should remove any
    /// directly assigned devices before the guest saves state.
    async fn prepare_for_hibernate(&self);

    /// Callback to cancel preparation for guest hibernation. This should
    /// restore any state updated via prepare_for_hibernate().
    async fn cancel_prepare_for_hibernate(&self);

    /// Callback when network settings are modified externally.
    async fn modify_network_settings(
        &mut self,
        instance_id: Guid,
        subordinate_instance_id: Option<Guid>,
    ) -> anyhow::Result<()>;

    /// Callback when network is added externally.
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
        keepalive_mode: KeepAliveConfig,
        mana_state: Option<&ManaSavedState>,
    ) -> anyhow::Result<RuntimeSavedState>;

    /// Callback when network is removed externally.
    async fn remove_network(&mut self, instance_id: Guid) -> anyhow::Result<()>;

    /// Callback after stopping the VM and all workers, in preparation for a VTL2 reboot.
    async fn unload_for_servicing(&mut self);

    /// Handles packet capture related operations.
    async fn packet_capture(
        &self,
        mut params: PacketCaptureParams<Socket>,
    ) -> anyhow::Result<PacketCaptureParams<Socket>>;

    /// Save the network state for restoration after servicing.
    async fn save(&mut self) -> Option<Vec<ManaSavedState>>;
}

/// A VM that has been loaded and can be run.
pub(crate) struct LoadedVm {
    pub partition_unit: PartitionUnit,
    /// The various guest memory objects.
    pub memory: underhill_mem::MemoryMappings,
    pub firmware_type: FirmwareType,
    pub isolation: IsolationType,
    // contain task handles which must be kept live
    pub chipset_devices: ChipsetDevices,
    // keep the unit task alive
    pub _vmtime: SpawnedUnit<VmTimeKeeper>,
    pub _halt_task: Task<()>,
    pub uevent_listener: Arc<UeventListener>,
    pub resolver: ResourceResolver,
    pub nvme_manager: Option<NvmeManager>,
    pub emuplat_servicing: EmuplatServicing,
    pub device_interfaces: Option<DeviceInterfaces>,
    pub vmbus_client: Option<vmbus_client::VmbusClient>,
    pub vmbus_filter: Option<vmbus_client::filter::ClientFilter>,
    pub vpci_relay: Option<vpci_relay::VpciRelay>,
    /// Memory map with IGVM types for each range.
    pub vtl0_memory_map: Vec<(MemoryRangeWithNode, MemoryMapEntryType)>,

    pub partition: Arc<UhPartition>,
    pub state_units: StateUnits,
    pub last_state_unit_stop: Option<ReferenceTime>,
    pub vmbus_server: Option<VmbusServerHandle>,
    // contain task handles which must be kept live
    pub host_vmbus_relay: Option<VmbusRelayHandle>,
    // channels are revoked when dropped, so make sure to keep them alive
    pub _vmbus_devices: Vec<SpawnedUnit<ChannelUnit<dyn VmbusDevice>>>,
    pub _ide_accel_devices: Vec<SpawnedUnit<ChannelUnit<storvsp::StorageDevice>>>,
    pub network_settings: Option<Box<dyn LoadedVmNetworkSettings>>,
    pub shutdown_relay: Option<(
        mesh::Receiver<Rpc<ShutdownParams, ShutdownResult>>,
        mesh::Sender<ShutdownRpc>,
    )>,

    pub vmgs: Option<(
        vmgs_broker::VmgsClient,
        disk_get_vmgs::save_restore::SavedBlockStorageMetadata,
        Task<()>,
    )>,

    // dependencies of the vtl2 settings service
    pub get_client: guest_emulation_transport::GuestEmulationTransportClient,
    pub device_platform_settings:
        guest_emulation_transport::api::platform_settings::DevicePlatformSettings,
    pub runtime_params: crate::loader::vtl2_config::RuntimeParameters,

    pub _input_distributor: SpawnedUnit<InputDistributor>,

    pub crash_notification_recv: mesh::Receiver<VtlCrash>,
    pub control_send: Arc<Mutex<Option<mesh::Sender<ControlRequest>>>>,

    pub _periodic_telemetry_task: Task<()>,

    pub nvme_keep_alive: KeepAliveConfig,
    pub mana_keep_alive: KeepAliveConfig,
    pub test_configuration: Option<TestScenarioConfig>,
    pub dma_manager: OpenhclDmaManager,
    pub config_timeout_in_seconds: u64,
    pub servicing_timeout_dump_collection_in_ms: u64,
}

pub struct LoadedVmState<T> {
    pub restart_rpc: FailableRpc<(), T>,
    pub servicing_state: ServicingState,
    pub vm_rpc: mesh::Receiver<UhVmRpc>,
    pub control_send: mesh::Sender<ControlRequest>,
}

impl LoadedVm {
    /// Start running the VM which will start running VTL0.
    pub async fn run<T: 'static + MeshPayload + Send>(
        mut self,
        threadpool: &AffinitizedThreadpool,
        autostart_vps: bool,
        correlation_id: Option<Guid>,
        mut vm_rpc: mesh::Receiver<UhVmRpc>,
        mut worker_rpc: mesh::Receiver<WorkerRpc<T>>,
    ) -> Option<LoadedVmState<T>> {
        if autostart_vps {
            self.start(correlation_id).await;
        }

        // VTL2 settings services
        let (device_config_send, mut device_config_recv) = mesh::channel();
        let _vtl2_settings_service_handle = {
            let initial_settings = self
                .device_platform_settings
                .general
                .vtl2_settings
                .as_ref()
                .map_or_else(Default::default, |settings| settings.dynamic.clone());

            let mut vtl2_settings_worker = Vtl2SettingsWorker::new(
                initial_settings,
                device_config_send,
                self.get_client.clone(),
                self.device_interfaces.take().unwrap(),
                self.config_timeout_in_seconds,
            );

            threadpool.spawn("VTL2 settings services", {
                let uevent_listener = self.uevent_listener.clone();
                async move { vtl2_settings_worker.run(&uevent_listener).await }
            })
        };

        let mut save_request_recv = self
            .get_client
            .take_save_request_recv()
            .await
            .expect("no failure");

        struct PendingShutdown {
            guest_response: PendingRpc<ShutdownResult>,
            send_result: Rpc<(), ShutdownResult>,
            is_hibernate: bool,
        }

        let mut pending_shutdown_response: Option<PendingShutdown> = None;

        let state = loop {
            enum Event<T> {
                WorkerRpc(WorkerRpc<T>),
                WorkerRpcGone,
                Vtl2ConfigNicRpc(Vtl2ConfigNicRpc),
                UhVmRpc(UhVmRpc),
                VtlCrash(VtlCrash),
                ServicingRequest(GuestSaveRequest),
                ShutdownRequest(Rpc<ShutdownParams, ShutdownResult>),
                ShutdownResponse(<PendingRpc<ShutdownResult> as Future>::Output),
                VpciRelayReady,
            }

            let event: Event<T> = futures::select! { // merge semantics
                message = worker_rpc.next() => message.map_or(Event::WorkerRpcGone, Event::WorkerRpc),
                message = device_config_recv.select_next_some() => Event::Vtl2ConfigNicRpc(message),
                message = vm_rpc.select_next_some() => Event::UhVmRpc(message),
                message = self.crash_notification_recv.select_next_some() => Event::VtlCrash(message),
                message = save_request_recv.select_next_some() => Event::ServicingRequest(message),
                _ = async {
                    if let Some(vpci_relay) = &mut self.vpci_relay {
                        vpci_relay.wait_ready().await
                    } else {
                        std::future::pending().await
                    }
                }.fuse() => Event::VpciRelayReady,
                message = async {
                    if self.shutdown_relay.is_none() {
                        std::future::pending::<()>().await;
                    }
                    let (recv, _) = self.shutdown_relay.as_mut().unwrap();
                    recv.select_next_some().await
                }.fuse() => Event::ShutdownRequest(message),
                message = async {
                    if let Some(PendingShutdown{guest_response, ..}) = &mut pending_shutdown_response {
                        guest_response.await
                    } else {
                        std::future::pending().await
                    }
                }.fuse() => Event::ShutdownResponse(message),
            };

            match event {
                Event::WorkerRpcGone => break None,
                Event::WorkerRpc(message) => match message {
                    WorkerRpc::Stop => break None,
                    WorkerRpc::Restart(rpc) => {
                        let state = async {
                            let running = self.stop().await;
                            match self
                                .save(None, KeepAliveConfig::Disabled, KeepAliveConfig::Disabled)
                                .await
                            {
                                Ok(servicing_state) => Some((rpc, servicing_state)),
                                Err(err) => {
                                    if running {
                                        self.start(None).await;
                                    }
                                    rpc.complete(Err(RemoteError::new(err)));
                                    None
                                }
                            }
                        }
                        .instrument(tracing::info_span!("restart", CVM_ALLOWED))
                        .await;

                        if let Some((rpc, servicing_state)) = state {
                            break Some(LoadedVmState {
                                restart_rpc: rpc,
                                servicing_state,
                                vm_rpc,
                                control_send: self.control_send.lock().take().unwrap(),
                            });
                        }
                    }
                    WorkerRpc::Inspect(deferred) => deferred.respond(|resp| {
                        resp.field("threadpool", threadpool)
                            .merge(&self.state_units);
                        resp.child("init_data", |req| {
                            req.respond().field("dps", &self.device_platform_settings);
                        });
                        resp.field("runtime_params", &self.runtime_params);
                        resp.field("get", &self.get_client);
                        resp.field("vmgs", self.vmgs.as_ref().map(|x| &x.0));
                        resp.field("network", &self.network_settings);
                        resp.field("nvme", &self.nvme_manager);
                        resp.field("resolver", &self.resolver);
                        resp.field(
                            "vtl0_memory_map",
                            inspect_helpers::vtl0_memory_map(&self.vtl0_memory_map),
                        );
                        resp.field("memory", &self.memory);
                        resp.field("dma_manager", &self.dma_manager);
                        resp.field("vmbus_client", &self.vmbus_client);
                        resp.field("vmbus_filter", &self.vmbus_filter);
                        resp.field("vpci_relay", &self.vpci_relay);
                        resp.field("mana_keepalive_mode", &self.mana_keep_alive);
                    }),
                },
                Event::Vtl2ConfigNicRpc(message) => {
                    handle_vtl2_config_rpc(message, &mut self, threadpool).await
                }
                Event::UhVmRpc(msg) => match msg {
                    UhVmRpc::Resume(rpc) => {
                        rpc.handle(async |()| {
                            if !self.state_units.is_running() {
                                self.start(None).await;
                                true
                            } else {
                                false
                            }
                        })
                        .await
                    }
                    UhVmRpc::Pause(rpc) => rpc.handle(async |()| self.stop().await).await,
                    UhVmRpc::Save(rpc) => {
                        rpc.handle_failable(async |()| {
                            let running = self.stop().await;
                            let r = self
                                .save(None, KeepAliveConfig::Disabled, KeepAliveConfig::Disabled)
                                .await;
                            if running {
                                self.start(None).await;
                            }
                            r.map(mesh::payload::encode)
                        })
                        .await
                    }
                    UhVmRpc::ClearHalt(rpc) => {
                        rpc.handle(async |()| self.partition_unit.clear_halt().await)
                            .await
                    }
                    UhVmRpc::PacketCapture(rpc) => {
                        rpc.handle_failable(async |params| {
                            let network_settings = self
                                .network_settings
                                .as_ref()
                                .context("No network settings have been set up")?;
                            network_settings.packet_capture(params).await
                        })
                        .await
                    }
                },
                Event::ServicingRequest(message) => {
                    // Explicitly destructure the message for easier tracking of its changes.
                    let GuestSaveRequest {
                        correlation_id,
                        timeout_hint,
                        capabilities_flags,
                    } = message;

                    // If the host provided timeout hint is >= uint16::max
                    // seconds, we treat that as a signal from the host that no
                    // timeout duration was set. We instead limit servicing to
                    // 200s in that case.
                    let timeout_hint = if timeout_hint >= Duration::from_secs(u16::MAX as u64) {
                        tracing::info!(
                            CVM_ALLOWED,
                            "host provided UINT16_MAX timeout hint, defaulting to 200s"
                        );
                        Duration::from_secs(200)
                    } else {
                        timeout_hint
                    };

                    let servicing_deadline = std::time::Instant::now() + timeout_hint;
                    tracing::info!(
                        CVM_ALLOWED,
                        correlation_id = %correlation_id,
                        timeout_hint_ms = timeout_hint.as_millis() as u64,
                        servicing_deadline = ?servicing_deadline,
                        "received servicing request from host"
                    );

                    match self
                        .handle_servicing_request(
                            correlation_id,
                            servicing_deadline,
                            capabilities_flags,
                        )
                        .await
                    {
                        Ok(true) => {
                            // Now do nothing. The host will restart VTL2 when it is ready.
                        }
                        Ok(false) => {
                            // Servicing failed. Continue running the VM.
                            continue;
                        }
                        Err(err) => {
                            tracing::error!(
                                CVM_ALLOWED,
                                error = err.as_ref() as &dyn std::error::Error,
                                "failed to notify host of servicing result"
                            );
                            // This is not recoverable, so tear down.
                            break None;
                        }
                    }
                }
                Event::ShutdownRequest(rpc) => {
                    if pending_shutdown_response.is_some() {
                        rpc.complete(ShutdownResult::AlreadyInProgress);
                        continue;
                    }
                    let (msg, send_result) = rpc.split();
                    let is_hibernate = matches!(msg.shutdown_type, ShutdownType::Hibernate);
                    if is_hibernate {
                        self.handle_hibernate_request().await;
                    }
                    let (_, send_guest) =
                        self.shutdown_relay.as_mut().expect("active shutdown_relay");
                    tracing::info!(CVM_ALLOWED, params = ?msg, "Relaying shutdown message");
                    pending_shutdown_response = Some(PendingShutdown {
                        guest_response: send_guest.call(ShutdownRpc::Shutdown, msg),
                        send_result,
                        is_hibernate,
                    });
                }
                Event::ShutdownResponse(result) => {
                    let response = match result {
                        Ok(result) => result,
                        Err(err) => {
                            tracing::error!(
                                CVM_ALLOWED,
                                error = &err as &dyn std::error::Error,
                                "Failed to relay shutdown notification to guest"
                            );
                            ShutdownResult::Failed(0x80004005)
                        }
                    };
                    let PendingShutdown {
                        send_result,
                        is_hibernate,
                        ..
                    } = pending_shutdown_response
                        .take()
                        .expect("no pending shutdown response");
                    if !matches!(response, ShutdownResult::Ok) {
                        tracing::warn!(CVM_ALLOWED, ?response, "Shutdown request failed");
                        if is_hibernate {
                            self.cancel_hibernate_request().await;
                        }
                    }
                    send_result.complete(response);
                }
                Event::VtlCrash(vtl_crash) => self.notify_of_vtl_crash(vtl_crash),
                Event::VpciRelayReady => {
                    if let Err(err) = self
                        .vpci_relay
                        .as_mut()
                        .unwrap()
                        .process(&self.chipset_devices, &mut self.state_units)
                        .await
                    {
                        tracing::error!(
                            CVM_ALLOWED,
                            error = err.as_ref() as &dyn std::error::Error,
                            "failed to process VPCI relay"
                        );
                    }
                }
            }
        };

        let _client_notify_send = self.partition_unit.teardown().await;

        // Terminate the vmbus relay before vmbus to avoid sending channel
        // revokes back to the host.
        if let Some(vmbus_relay) = self.host_vmbus_relay {
            vmbus_relay.teardown().await;
        }

        if let Some(vmbus) = self.vmbus_server {
            vmbus.remove().await.shutdown().await;
        }

        state
    }

    /// Handles a servicing request from the host.
    ///
    /// Returns `true` if servicing was successful (in which case the VM will be
    /// terminated any moment), `false` if it failed non-destructively the VM
    /// should keep running.
    async fn handle_servicing_request(
        &mut self,
        correlation_id: Guid,
        deadline: std::time::Instant,
        capabilities_flags: SaveGuestVtl2StateFlags,
    ) -> anyhow::Result<bool> {
        if let Some(TestScenarioConfig::SaveStuck) = self.test_configuration {
            tracing::info!("Test configuration SERVICING_SAVE_STUCK is set. Waiting indefinitely.");
            std::future::pending::<()>().await;
        }

        let running = self.state_units.is_running();
        let success = match self
            .handle_servicing_inner(correlation_id, deadline, capabilities_flags)
            .await
            .and_then(|state| {
                if let Some(TestScenarioConfig::SaveFail) = self.test_configuration {
                    tracing::info!(
                        "Test configuration SERVICING_SAVE_FAIL is set. Failing the save."
                    );
                    return Err(anyhow::anyhow!("Simulated servicing save failure"));
                }
                Ok(state)
            }) {
            Ok(state) => {
                self.get_client
                    .send_servicing_state(mesh::payload::encode(state))
                    .await?;

                true
            }
            Err(err) => {
                tracing::error!(
                    CVM_ALLOWED,
                    error = err.as_ref() as &dyn std::error::Error,
                    "error while handling servicing"
                );
                self.get_client
                    .send_servicing_failure(format_args!("{:#}", err))
                    .await
                    .context("failed to notify host of servicing-while-paused failure")?;

                if running {
                    self.start(Some(correlation_id)).await;
                }
                false
            }
        };

        Ok(success)
    }

    async fn handle_servicing_inner(
        &mut self,
        correlation_id: Guid,
        deadline: std::time::Instant,
        capabilities_flags: SaveGuestVtl2StateFlags,
    ) -> anyhow::Result<ServicingState> {
        if self.isolation.is_isolated() {
            anyhow::bail!("Servicing is not yet supported for isolated VMs");
        }

        // Start a servicing timeout thread on its own executor in a new thread.
        // Do this to avoid any tasks that may block the current threadpool
        // executor, and drop the task when this function returns which will
        // dismiss the timeout panic.
        //
        // This helps catch issues where save may hang, and allows the existing
        // machinery to send the dump to the host when this process crashes.
        // Note that we choose to not use a livedump call like the firmware
        // watchdog handlers, as we expect any hang to be a fatal error for
        // OpenHCL, whereas the firmware watchdog is a failure inside the guest,
        // not necessarily inside OpenHCL.
        let (_servicing_timeout_thread, timeout_driver) =
            pal_async::DefaultPool::spawn_on_thread("servicing-timeout-executor");
        let dump_collection_duration =
            Duration::from_millis(self.servicing_timeout_dump_collection_in_ms);
        let _servicing_timeout =
            timeout_driver
                .clone()
                .spawn("servicing-timeout-task", async move {
                    let mut timer = pal_async::timer::PolledTimer::new(&timeout_driver);
                    // Subtract the configured dump collection duration from the
                    // host provided timeout hint to allow for time for the dump
                    // to be sent to the host before termination.
                    //
                    // This has a default value of 500ms - see
                    // `OPENHCL_SERVICING_TIMEOUT_DUMP_COLLECTION_IN_MS`.
                    let duration = deadline
                        .checked_duration_since(std::time::Instant::now())
                        .map(|d| d.saturating_sub(dump_collection_duration))
                        .unwrap_or(Duration::from_secs(0));
                    timer.sleep(duration).await;
                    tracing::error!(
                        CVM_ALLOWED,
                        "servicing operation timed out, triggering panic"
                    );
                    panic!("servicing operation timed out");
                });

        // NOTE: This is set via the corresponding env arg, as this feature is
        // experimental.
        if !capabilities_flags.enable_nvme_keepalive() {
            self.nvme_keep_alive = KeepAliveConfig::Disabled
        };

        if !capabilities_flags.enable_mana_keepalive() {
            self.mana_keep_alive = KeepAliveConfig::Disabled
        };

        // Do everything before the log flush under a span.
        let r = async {
            if !self.stop().await {
                // This should only occur if you tried to initiate a
                // servicing operation after manually pausing underhill
                // via `ohcldiag-dev`.
                //
                // This is something that we _could_ enable, but it'd
                // require additional plumbing, so we'll just disallow
                // this for now.
                anyhow::bail!("cannot service underhill while paused");
            }

            let mut state = self
                .save(
                    Some(deadline),
                    self.nvme_keep_alive.clone(),
                    self.mana_keep_alive.clone(),
                )
                .await?;
            state.init_state.correlation_id = Some(correlation_id);

            // Unload any network devices.
            let shutdown_mana = async {
                if let Some(network_settings) = self.network_settings.as_mut() {
                    network_settings
                        .unload_for_servicing()
                        .instrument(
                            tracing::info_span!("shutdown_mana", CVM_ALLOWED, %correlation_id),
                        )
                        .await;
                }
            };

            // Reset all user-mode NVMe devices.
            let shutdown_nvme = async {
                if let Some(nvme_manager) = self.nvme_manager.take() {
                    nvme_manager
                        .shutdown(self.nvme_keep_alive.is_enabled())
                        .instrument(tracing::info_span!(
                            "shutdown_nvme_vfio",
                            CVM_ALLOWED,
                            correlation_id = %correlation_id,
                            nvme_keep_alive_enabled = self.nvme_keep_alive.is_enabled()
                        ))
                        .await;
                }
            };

            // Unbind drivers from the PCI devices to prepare for a kernel
            // restart.
            let shutdown_pci = async {
                pci_shutdown::shutdown_pci_devices()
                    .instrument(
                        tracing::info_span!("shutdown_pci_devices", CVM_ALLOWED, %correlation_id),
                    )
                    .await
            };

            // Save the persisted state used by the next openhcl_boot.
            // In the future, this could be extended to include other devices (e.g. MANA).
            let nvme_vp_interrupt_state =
                crate::nvme_manager::save_restore_helpers::nvme_interrupt_state(
                    state.init_state.nvme_state.as_ref().map(|n| &n.nvme_state),
                );

            crate::loader::vtl2_config::write_persisted_info(
                self.runtime_params.parsed_openhcl_boot(),
                nvme_vp_interrupt_state,
            )
            .context("failed to write persisted info")?;

            let (r, (), ()) = (shutdown_pci, shutdown_mana, shutdown_nvme).join().await;
            r?;

            Ok(state)
        }
        .instrument(tracing::info_span!("servicing_save_vtl2", CVM_ALLOWED, %correlation_id))
        .await;

        let mut state = match r {
            Ok(state) => state,
            Err(err) => {
                self.resume_drivers();
                return Err(err);
            }
        };

        // Tell the initial process to flush all logs. Any logs
        // emitted after this point may be lost.
        state.init_state.flush_logs_result = Some({
            // Only wait up to a half second (which is still
            // a long time!) to prevent delays from
            // introducing longer blackouts.
            let ctx = CancelContext::new().with_timeout(Duration::from_millis(500));

            let now = std::time::Instant::now();
            let call = self
                .control_send
                .lock()
                .as_ref()
                .unwrap()
                .call(ControlRequest::FlushLogs, ctx);

            let error = call
                .await
                .map_err(anyhow::Error::from)
                .and_then(|x| x.map_err(anyhow::Error::from))
                .err()
                .map(|err| format!("{err:#}"));

            servicing::FlushLogsResult {
                duration_us: now.elapsed().as_micros() as u64,
                error,
            }
        });

        Ok(state)
    }

    async fn handle_hibernate_request(&self) {
        if let Some(network_settings) = &self.network_settings {
            network_settings
                .prepare_for_hibernate()
                .instrument(tracing::info_span!(
                    "prepare_for_guest_hibernate",
                    CVM_ALLOWED
                ))
                .await;
        }
    }

    async fn cancel_hibernate_request(&self) {
        if let Some(network_settings) = &self.network_settings {
            network_settings
                .cancel_prepare_for_hibernate()
                .instrument(tracing::info_span!(
                    "rollback_prepare_for_guest_hibernate",
                    CVM_ALLOWED
                ))
                .await;
        }
    }

    async fn start(&mut self, correlation_id: Option<Guid>) {
        self.state_units.start().await;

        // Log the boot/blackout time.
        let reference_time = ReferenceTime::new(self.partition.reference_time());
        if let Some(stopped) = self.last_state_unit_stop {
            let blackout_time = reference_time.since(stopped);
            tracing::info!(CVM_ALLOWED,
                correlation_id = %correlation_id.unwrap_or(Guid::ZERO),
                blackout_time_ms = blackout_time.map(|t| t.as_millis() as u64),
                blackout_time = blackout_time
                    .map_or_else(|| "unknown".to_string(), |t| format!("{:?}", t))
                    .as_str(),
                "resuming VM"
            );
        } else {
            // Assume we started at reference time 0.
            let boot_time = reference_time.since(ReferenceTime::new(0));
            tracing::info!(
                CVM_ALLOWED,
                boot_time_ms = boot_time.map(|t| t.as_millis() as u64),
                boot_time = boot_time
                    .map_or_else(|| "unknown".to_string(), |t| format!("{:?}", t))
                    .as_str(),
                "starting VM"
            )
        }
    }

    /// Returns true if the VM was previously running.
    async fn stop(&mut self) -> bool {
        if self.state_units.is_running() {
            self.last_state_unit_stop = Some(ReferenceTime::new(self.partition.reference_time()));
            tracing::info!(CVM_ALLOWED, "stopping VM");
            self.state_units.stop().await;
            true
        } else {
            false
        }
    }

    /// Called after a failed servicing operation.
    ///
    /// FUTURE: model the drivers as "driver" state units (as opposed to guest
    /// VM state units) so that we have a consistent way to model their state
    /// transitions.
    fn resume_drivers(&mut self) {
        if let Some(client) = &mut self.vmbus_client {
            client.start();
        }

        // BUGBUG: resume the other drivers. This only becomes a problem once
        // nvme keepalive is enabled, since otherwise no other drivers have been
        // stopped.
    }

    async fn save(
        &mut self,
        _deadline: Option<std::time::Instant>,
        nvme_keepalive_mode: KeepAliveConfig,
        mana_keepalive_mode: KeepAliveConfig,
    ) -> anyhow::Result<ServicingState> {
        assert!(!self.state_units.is_running());

        let emuplat = (self.emuplat_servicing.save()).context("emuplat save failed")?;

        // Only save dma manager state if we are expected to keep VF devices
        // alive across save. Otherwise, don't persist the state at all, as
        // there should be no live DMA across save.
        //
        // This has to happen before saving the network state, otherwise its allocations
        // are marked as Free and are unable to be restored.
        let dma_manager_state =
            if nvme_keepalive_mode.is_enabled() || mana_keepalive_mode.is_enabled() {
                use vmcore::save_restore::SaveRestore;
                Some(self.dma_manager.save().context("dma_manager save failed")?)
            } else {
                None
            };

        // Only save NVMe state when there are NVMe controllers and keep alive
        // was enabled.
        let nvme_state = if let Some(n) = &self.nvme_manager {
            // DEVNOTE: A subtlety here is that the act of saving the NVMe state also causes the driver
            // to enter a state where subsequent teardown operations will noop. There is a STRONG
            // correlation between save/restore and keepalive.
            n.save(nvme_keepalive_mode.is_enabled())
                .instrument(tracing::info_span!(
                    "nvme_manager_save",
                    nvme_keepalive_mode = %nvme_keepalive_mode.is_enabled(),
                    CVM_ALLOWED
                ))
                .await
                .map(|s| NvmeSavedState { nvme_state: s })
        } else {
            None
        };

        let units = self.save_units().await.context("state unit save failed")?;

        let mana_state = if let Some(network_settings) = &mut self.network_settings
            && mana_keepalive_mode.is_enabled()
        {
            network_settings.save().await
        } else {
            None
        };

        let vmgs = if let Some((vmgs_thin_client, vmgs_disk_metadata, _)) = self.vmgs.as_ref() {
            Some((
                vmgs_thin_client.save().await.context("vmgs save failed")?,
                vmgs_disk_metadata.clone(),
            ))
        } else {
            None
        };

        let vmbus_client = if let Some(vmbus_client) = &mut self.vmbus_client {
            vmbus_client.stop().await;
            Some(vmbus_client.save().await)
        } else {
            None
        };

        let mut state = ServicingState {
            init_state: servicing::ServicingInitState {
                firmware_type: self.firmware_type.into(),
                vm_stop_reference_time: self.last_state_unit_stop.unwrap().as_100ns(),
                correlation_id: None,
                emuplat,
                flush_logs_result: None,
                vmgs,
                overlay_shutdown_device: self.shutdown_relay.is_some(),
                nvme_state,
                dma_manager_state,
                vmbus_client,
                mana_state,
            },
            units,
        };

        state
            .fix_pre_save()
            .context("failed to fix up servicing state before save")?;
        Ok(state)
    }

    #[instrument(skip(self), fields(CVM_ALLOWED))]
    async fn save_units(&mut self) -> anyhow::Result<Vec<SavedStateUnit>> {
        Ok(self.state_units.save().await?)
    }

    #[instrument(skip(self, saved_state), fields(CVM_ALLOWED))]
    pub async fn restore_units(&mut self, saved_state: Vec<SavedStateUnit>) -> anyhow::Result<()> {
        self.state_units.restore(saved_state).await?;
        Ok(())
    }

    fn notify_of_vtl_crash(&self, vtl_crash: VtlCrash) {
        tracelimit::info_ratelimited!(CVM_ALLOWED, "Notifying the host of the guest system crash");

        let VtlCrash {
            vp_index,
            last_vtl,
            control,
            parameters,
        } = vtl_crash;
        self.get_client.notify_of_vtl_crash(
            vp_index.index(),
            last_vtl.into(),
            control.into(),
            parameters,
        );
    }

    async fn add_vf_manager(
        &mut self,
        threadpool: &AffinitizedThreadpool,
        instance_id: Guid,
        subordinate_instance_id: Option<Guid>,
        max_sub_channels: Option<u16>,
    ) -> anyhow::Result<()> {
        // Network Settings may not exist, if the VM was created without network_settings
        if self.network_settings.is_none() {
            return Err(NetworkSettingsError::NetworkSettingsMissing.into());
        }

        let save_state = self
            .network_settings
            .as_mut()
            .unwrap()
            .add_network(
                instance_id,
                subordinate_instance_id,
                max_sub_channels,
                threadpool,
                &self.uevent_listener,
                &None, // VF getting added; no existing state
                self.partition.clone(),
                &self.state_units,
                &self.vmbus_server,
                self.dma_manager.client_spawner(),
                self.isolation.is_isolated(),
                self.mana_keep_alive.clone(),
                None, // No existing mana state
            )
            .await?;

        self.state_units.start_stopped_units().await;
        self.emuplat_servicing.netvsp_state.push(save_state);

        Ok(())
    }

    async fn remove_vf_manager(&mut self, instance_id: Guid) -> anyhow::Result<()> {
        if self.network_settings.is_none() {
            return Err(NetworkSettingsError::NetworkSettingsMissing.into());
        }

        self.network_settings
            .as_mut()
            .unwrap()
            .remove_network(instance_id)
            .await?;

        // Remove Netvsp RuntimeSavedState
        if let Some(index) = self
            .emuplat_servicing
            .netvsp_state
            .iter()
            .position(|value| value.instance_id == instance_id)
        {
            self.emuplat_servicing.netvsp_state.swap_remove(index);
        } else {
            return Err(NetworkSettingsError::RuntimeSavedStateMissing(instance_id).into());
        }

        Ok(())
    }
}

mod inspect_helpers {
    use super::*;

    fn inspect_memory_map_entry_type(typ: &MemoryMapEntryType) -> impl Inspect + '_ {
        // TODO: inspect::AsDebug would work here once
        // https://github.com/kupiakos/open-enum/pull/15 is merged.
        inspect::adhoc(|req| match *typ {
            MemoryMapEntryType::MEMORY => req.value("MEMORY"),
            MemoryMapEntryType::PERSISTENT => req.value("PERSISTENT"),
            MemoryMapEntryType::PLATFORM_RESERVED => req.value("PLATFORM_RESERVED"),
            MemoryMapEntryType::VTL2_PROTECTABLE => req.value("VTL2_PROTECTABLE"),
            _ => req.value(typ.0),
        })
    }

    pub(super) fn vtl0_memory_map(
        memory: &[(MemoryRangeWithNode, MemoryMapEntryType)],
    ) -> impl Inspect + '_ {
        inspect::iter_by_key(memory.iter().map(|(entry, typ)| {
            (
                entry.range,
                inspect::adhoc(|req| {
                    req.respond()
                        .hex("length", entry.range.len())
                        .field("type", inspect_memory_map_entry_type(typ))
                        .field("vnode", entry.vnode);
                }),
            )
        }))
    }
}
