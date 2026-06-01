// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::dispatch::vtl2_settings_worker::wait_for_pci_path;
use crate::options::KeepAliveConfig;
use crate::vpci::HclVpciBusControl;
use anyhow::Context;
use anyhow::anyhow;
use async_trait::async_trait;
use futures::StreamExt;
use futures::lock::Mutex;
use futures::stream::iter;
use futures_concurrency::stream::Merge;
use guest_emulation_transport::GuestEmulationTransportClient;
use guid::Guid;
use inspect::Inspect;
use mana_driver::mana::ManaDevice;
use mana_driver::mana::VportState;
use mana_driver::save_restore::ManaSavedState;
use mesh::payload::Protobuf;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use net_backend::DisconnectableEndpoint;
use net_backend::DisconnectableEndpointControl;
use net_backend::Endpoint;
use net_backend_resources::mac_address::MacAddress;
use net_mana::GuestDmaMode;
use net_packet_capture::PacketCaptureEndpoint;
use net_packet_capture::PacketCaptureEndpointControl;
use net_packet_capture::PacketCaptureParams;
use pal_async::task::Spawn;
use pal_async::task::Task;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
pub use save_restore::RuntimeSavedState;
pub use save_restore::state::SavedState;
use socket2::Socket;
use std::collections::HashMap;
use std::future::pending;
use std::path::Path;
use std::sync::Arc;
use std::task::Poll;
use std::task::ready;
use tracing::Instrument;
use uevent::UeventListener;

use user_driver::vfio::PciDeviceResetMethod;
use user_driver::vfio::VfioDevice;
use user_driver::vfio::VfioDmaClients;
use user_driver::vfio::vfio_set_device_reset_method;
use vmcore::vm_task::VmTaskDriverSource;
use vpci::bus_control::VpciBusControl;
use vpci::bus_control::VpciBusEvent;

/// Default timeout for actions communicating with other components where an action
/// is expected to take time, but still complete in a reasonable window.
const MAX_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug)]
enum HclNetworkVfManagerMessage {
    AddGuestVFManager(
        Rpc<mesh::Sender<HclNetworkVFUpdateNotification>, HclNetworkVFManagerGuestState>,
    ),
    AddVtl0VF,
    RemoveVtl0VF,
    ShutdownBegin(bool),
    ShutdownComplete(Rpc<bool, ()>),
    UpdateVtl0VF(Rpc<Option<HclVpciBusControl>, ()>),
    HideVtl0VF(Rpc<bool, ()>),
    Inspect(inspect::Deferred),
    PacketCapture(FailableRpc<PacketCaptureParams<Socket>, PacketCaptureParams<Socket>>),
    SaveState(Rpc<(), VfManagerSaveResult>),
}

#[expect(clippy::large_enum_variant)]
#[derive(Debug)]
enum VfManagerSaveResult {
    Saved(ManaSavedState),
    DeviceMissing,
    SaveFailed,
}

async fn create_mana_device(
    driver_source: &VmTaskDriverSource,
    pci_id: &str,
    vtl2_vf_instance_id: &Guid,
    vp_count: u32,
    max_sub_channels: u16,
    keepalive_mode: KeepAliveConfig,
    dma_clients: VfioDmaClients,
    mut mana_state: Option<&ManaSavedState>,
) -> anyhow::Result<ManaDevice<VfioDevice>> {
    let vtl2_vfid = vfid_from_guid(vtl2_vf_instance_id);
    // This guards from situations where we have saved state from keepalive
    // but the host does not support restoring it. In this case we log a warning,
    // free the memory, and continue with a fresh device.
    if mana_state.is_some() && !keepalive_mode.is_enabled() {
        tracing::warn!(
            vtl2_vfid,
            "have saved state from keepalive but restoring on an unsupported host"
        );

        // Re-attach pending buffers, but discard them so that they get freed.
        let dma_client = match &dma_clients {
            VfioDmaClients::EphemeralOnly(_) | VfioDmaClients::PersistentOnly(_) => {
                anyhow::bail!("must have both clients to free previously attached buffers")
            }
            VfioDmaClients::Split { persistent, .. } => persistent,
        };
        let _ = dma_client.attach_pending_buffers();

        // Remove the mana saved state so that we don't go through restore path.
        let _ = mana_state.take();
    }

    if keepalive_mode.is_enabled() && mana_state.is_some() {
        try_create_mana_device(
            driver_source,
            pci_id,
            vtl2_vf_instance_id,
            vp_count,
            max_sub_channels,
            dma_clients.clone(),
            mana_state,
        )
        .await
    } else {
        // Disable FLR on vfio attach/detach; this allows faster system
        // startup/shutdown with the caveat that the device needs to be properly
        // sent through the shutdown path during servicing operations, as that is
        // the only cleanup performed. If the device fails to initialize, turn FLR
        // on and try again, so that the reset is invoked on the next attach.
        let update_reset = |method: PciDeviceResetMethod| {
            if let Err(err) = vfio_set_device_reset_method(pci_id, method) {
                tracing::warn!(
                    vtl2_vfid,
                    ?method,
                    err = &err as &dyn std::error::Error,
                    "Failed to update reset_method"
                );
            }
        };
        let mut last_err: Option<anyhow::Error> = None;
        let mut created: Option<ManaDevice<VfioDevice>> = None;

        for reset_method in [PciDeviceResetMethod::NoReset, PciDeviceResetMethod::Flr] {
            update_reset(reset_method);
            match try_create_mana_device(
                driver_source,
                pci_id,
                vtl2_vf_instance_id,
                vp_count,
                max_sub_channels,
                dma_clients.clone(),
                None,
            )
            .await
            {
                Ok(device) => {
                    if !matches!(reset_method, PciDeviceResetMethod::NoReset) {
                        // Restore the faster path for subsequent attaches.
                        update_reset(PciDeviceResetMethod::NoReset);
                    }
                    created = Some(device);
                    break;
                }
                Err(err) => {
                    tracing::error!(
                        vtl2_vfid,
                        pci_id,
                        ?reset_method,
                        err = err.as_ref() as &dyn std::error::Error,
                        "failed to create mana device"
                    );
                    last_err = Some(err);
                }
            }
        }

        match created {
            Some(device) => Ok(device),
            None => Err(last_err.unwrap()).context("failed to create mana device"),
        }
    }
}

async fn try_create_mana_device(
    driver_source: &VmTaskDriverSource,
    pci_id: &str,
    vtl2_vf_instance_id: &Guid,
    vp_count: u32,
    max_sub_channels: u16,
    dma_clients: VfioDmaClients,
    mana_state: Option<&ManaSavedState>,
) -> anyhow::Result<ManaDevice<VfioDevice>> {
    let vtl2_vfid = vfid_from_guid(vtl2_vf_instance_id);
    // Restore the device if we have saved state from servicing, otherwise create a new one.
    let device = if mana_state.is_some() {
        tracing::debug!(vtl2_vfid, "Restoring VFIO device from saved state");
        VfioDevice::restore(driver_source, pci_id, true, dma_clients)
            .instrument(tracing::info_span!(
                "restore_mana_vfio_device",
                vtl2_vfid,
                pci_id
            ))
            .await
            .with_context(|| format!("failed to restore vfio device for {}", pci_id))?
    } else {
        VfioDevice::new(driver_source, pci_id, dma_clients)
            .instrument(tracing::info_span!(
                "new_mana_vfio_device",
                vtl2_vfid,
                pci_id
            ))
            .await
            .with_context(|| format!("failed to open vfio device for {}", pci_id))?
    };

    tracing::info!(vtl2_vfid, pci_id, "Creating MANA device");
    ManaDevice::new(
        &driver_source.simple(),
        device,
        vp_count,
        max_sub_channels + 1,
        mana_state.map(|state| &state.mana_device),
    )
    .instrument(tracing::info_span!("new_mana_device", vtl2_vfid, pci_id))
    .await
    .context("failed to initialize mana device")
}

fn vtl0_vfid_from_bus_control(vtl0_bus_control: &Vtl0Bus) -> Option<u32> {
    match vtl0_bus_control {
        Vtl0Bus::Present(bus_control) => Some(vfid_from_guid(&bus_control.instance_id())),
        _ => None,
    }
}

fn vtl2_vfid_from_bus_control(vtl2_bus_control: &HclVpciBusControl) -> u32 {
    vfid_from_guid(&vtl2_bus_control.instance_id())
}

fn vfid_from_guid(id: &Guid) -> u32 {
    id.data1
}

#[derive(Clone, Debug)]
struct HclNetworkVFManagerGuestState {
    offered_to_guest: Arc<Mutex<bool>>,
    vtl0_vfid: Arc<Mutex<Option<u32>>>,
}

impl HclNetworkVFManagerGuestState {
    pub fn new(vtl0_bus_control: &Vtl0Bus) -> Self {
        Self {
            offered_to_guest: Arc::new(Mutex::new(false)),
            vtl0_vfid: Arc::new(Mutex::new(vtl0_vfid_from_bus_control(vtl0_bus_control))),
        }
    }

    pub async fn is_offered_to_guest(&self) -> bool {
        *self.offered_to_guest.lock().await
    }

    pub async fn vtl0_vfid(&self) -> Option<u32> {
        *self.vtl0_vfid.lock().await
    }
}

enum Vtl0Bus {
    NotPresent,
    Present(HclVpciBusControl),
    HiddenNotPresent,
    HiddenPresent(HclVpciBusControl),
}
impl std::fmt::Display for Vtl0Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Vtl0Bus::NotPresent => write!(f, "NotPresent"),
            Vtl0Bus::Present(bus_control) => {
                write!(f, "Present(vtl0_vfid={})", bus_control.instance_id().data1)
            }
            Vtl0Bus::HiddenNotPresent => write!(f, "HiddenNotPresent"),
            Vtl0Bus::HiddenPresent(bus_control) => {
                write!(
                    f,
                    "HiddenPresent(vtl0_vfid={})",
                    bus_control.instance_id().data1
                )
            }
        }
    }
}

// The worker's lifecycle is tracked across a few orthogonal pieces of state
// rather than a single enum:
//
// - `self.is_shutdown_active` gates guest-driven transitions.
// - `Vtl2DeviceState` tracks whether the VTL2 VF is present, missing, or in the
//   middle of a reconfiguration retry loop.
// - `self.vtl0_bus_control` tracks whether the VTL0 VF exists and whether it is
//   currently hidden from the guest.
// - `self.guest_state` is the guest-visible projection of that internal state.
// - `self.mana_device.is_some()` means the worker currently owns a live VTL2
//   MANA device and its endpoints may be connected.
//
// The helper methods below document the assumptions and post-state they rely on
// so a reader can reason about transitions without having to keep the whole
// `run()` loop in mind.

#[derive(Inspect)]
struct HclNetworkVFManagerWorker {
    #[inspect(skip)]
    driver_source: VmTaskDriverSource,
    is_shutdown_active: bool,
    mana_device: Option<ManaDevice<VfioDevice>>,
    #[inspect(skip)]
    endpoint_controls: Vec<DisconnectableEndpointControl>,
    #[inspect(skip)]
    pkt_capture_controls: Option<Vec<PacketCaptureEndpointControl>>,
    #[inspect(skip)]
    guest_state: HclNetworkVFManagerGuestState,
    #[inspect(skip)]
    guest_state_notifications: Vec<mesh::Sender<HclNetworkVFUpdateNotification>>,
    max_sub_channels: u16,
    #[inspect(skip)]
    messages: Option<mesh::Receiver<HclNetworkVfManagerMessage>>,
    #[inspect(skip)]
    save_state: RuntimeSavedState,
    #[inspect(skip)]
    uevent_handler: HclNetworkVfManagerUeventHandler,
    vp_count: u32,
    #[inspect(skip)]
    vtl0_bus_control: Vtl0Bus,
    #[inspect(skip)]
    vtl2_bus_control: HclVpciBusControl,
    vtl2_pci_id: String,
    #[inspect(skip)]
    dma_mode: GuestDmaMode,
    #[inspect(skip)]
    dma_clients: VfioDmaClients,
    #[inspect(skip)]
    vf_reset_request_receiver: Option<mesh::Receiver<()>>,
    #[inspect(skip)]
    network_adapter_index: NetworkAdapterIndex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Vtl2DeviceState {
    /// The VTL2 VF is bound and VTL0 updates may be applied immediately.
    Present,
    /// The VTL2 VF is gone; VTL0 changes are recorded locally until it returns.
    Missing,
    /// The VTL2 VF was shut down for recovery and a restart retry is pending.
    Reconfiguring,
}

#[derive(Clone, Copy, Debug)]
struct VfReconfigBackoff {
    deadline: Instant,
    sleep: std::time::Duration,
    attempts: u64,
}

const RECONFIG_INITIAL_SLEEP: std::time::Duration = std::time::Duration::from_millis(100);
const RECONFIG_MAX_SLEEP: std::time::Duration = std::time::Duration::from_secs(2);
const RECONFIG_MAX_ATTEMPTS: u64 = 300; // ~10 minutes of retries at max backoff

impl HclNetworkVFManagerWorker {
    pub fn new(
        mana_device: ManaDevice<VfioDevice>,
        save_state: RuntimeSavedState,
        vtl2_pci_id: String,
        vtl2_bus_control: HclVpciBusControl,
        vtl0_bus_control: Option<HclVpciBusControl>,
        uevent_handler: HclNetworkVfManagerUeventHandler,
        driver_source: &VmTaskDriverSource,
        endpoint_controls: Vec<DisconnectableEndpointControl>,
        vp_count: u32,
        max_sub_channels: u16,
        dma_mode: GuestDmaMode,
        dma_clients: VfioDmaClients,
        network_adapter_index: NetworkAdapterIndex,
    ) -> (Self, mesh::Sender<HclNetworkVfManagerMessage>) {
        let (tx_to_worker, worker_rx) = mesh::channel();
        let vtl0_bus_control = if save_state.hidden_vtl0.lock().unwrap_or(false) {
            vtl0_bus_control
                .map(Vtl0Bus::HiddenPresent)
                .unwrap_or(Vtl0Bus::HiddenNotPresent)
        } else {
            vtl0_bus_control
                .map(Vtl0Bus::Present)
                .unwrap_or(Vtl0Bus::NotPresent)
        };
        (
            Self {
                driver_source: driver_source.clone(),
                is_shutdown_active: false,
                mana_device: Some(mana_device),
                endpoint_controls,
                pkt_capture_controls: None,
                guest_state: HclNetworkVFManagerGuestState::new(&vtl0_bus_control),
                guest_state_notifications: Vec::new(),
                max_sub_channels,
                messages: Some(worker_rx),
                save_state,
                uevent_handler,
                vp_count,
                vtl0_bus_control,
                vtl2_bus_control,
                vtl2_pci_id,
                dma_mode,
                dma_clients,
                vf_reset_request_receiver: None,
                network_adapter_index,
            },
            tx_to_worker,
        )
    }

    pub async fn connect_endpoints(&mut self) -> anyhow::Result<Vec<(MacAddress, u32)>> {
        let device = self.mana_device.as_ref().expect("valid endpoint");
        let indices = (0..device.num_vports()).collect::<Vec<u32>>();
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        let result = futures::future::try_join_all(
            indices.iter().zip(self.endpoint_controls.iter_mut()).map(
                |(index, endpoint_control)| {
                    let vport_state = VportState::new(
                        self.save_state.direction_to_vtl0(*index),
                        Some(self.save_state.vport_callback(*index)),
                    );
                    let pending_device =
                        device.new_vport(*index, Some(vport_state), device.dev_config());
                    async {
                        let vport = pending_device
                            .await
                            .with_context(|| format!("failed to create mana vport {vtl2_vfid}"))?;
                        let mac_address = vport.mac_address();
                        let adapter_index = self.network_adapter_index.next(&mac_address);
                        vport.set_serial_no(adapter_index).await.with_context(|| {
                            format!("failed to set vport serial number {mac_address} {vtl2_vfid}")
                        })?;
                        let mana_ep = Box::new(
                            net_mana::ManaEndpoint::new(
                                self.driver_source.simple(),
                                vport,
                                self.dma_mode,
                            )
                            .await,
                        );
                        let (pkt_capture_ep, control) =
                            PacketCaptureEndpoint::new(mana_ep, mac_address.to_string());
                        endpoint_control
                            .connect(Box::new(pkt_capture_ep))
                            .with_context(|| {
                                format!("failed to connect new endpoint {mac_address} {vtl2_vfid}")
                            })?;
                        tracing::info!(vtl2_vfid, %mac_address, %adapter_index, "Network endpoint connected");
                        anyhow::Ok((mac_address, adapter_index, control))
                    }
                },
            ),
        )
        .instrument(tracing::info_span!(
            "connecting endpoints",
            vtl2_vfid,
            num_endpoints = indices.len()
        ))
        .await?;
        let (endpoint_info, pkt_capture_controls): (Vec<(MacAddress, u32)>, Vec<_>) = result
            .into_iter()
            .map(|(mac, idx, ctrl)| ((mac, idx), ctrl))
            .unzip();
        self.pkt_capture_controls = Some(pkt_capture_controls);
        Ok(endpoint_info)
    }

    /// Notifies all guest-facing listeners that VF state changed.
    ///
    /// The notification does not itself mutate worker state; callers are
    /// expected to update `self.guest_state` before invoking it so observers see
    /// a consistent view.
    async fn send_vf_state_change_notifications(&self) -> anyhow::Result<()> {
        let all_results =
            futures::future::join_all(self.guest_state_notifications.iter().map(async |update| {
                update
                    .call(HclNetworkVFUpdateNotification::Update, ())
                    .await
                    .map_err(anyhow::Error::from)
            }));
        let mut ctx = mesh::CancelContext::new().with_timeout(MAX_WAIT_TIMEOUT);
        ctx.until_cancelled(all_results)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map(drop)
    }

    /// Gives the guest a chance to react to VTL0 VF removal and then revokes it.
    ///
    /// This method intentionally clears `guest_state.offered_to_guest` before
    /// waiting on guest notifications so repeat removal requests are harmless.
    ///
    /// On return, the worker will no longer treat the VF as offered, regardless
    /// of whether notification or revoke operations encountered errors.
    async fn try_notify_guest_and_revoke_vtl0_vf(&mut self, bus_control: &Vtl0Bus) {
        if !self.guest_state.is_offered_to_guest().await {
            return;
        }

        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);

        // Make removal request a no-op by setting offered to false. The actual removal will be done at the end of this
        // method.
        *self.guest_state.offered_to_guest.lock().await = false;
        // Give the network stack a chance to prepare for the removal.
        if let Err(err) = self
            .send_vf_state_change_notifications()
            .instrument(tracing::info_span!(
                "sending VTL0 VF removal notice",
                vtl2_vfid,
                vtl0_bus = %bus_control))
            .await
        {
            tracing::error!(
                vtl2_vfid,
                err = err.as_ref() as &dyn std::error::Error,
                "Notify VTL0 VF removal"
            );

            // Force data path to VTL2 on error.
            if let Err(err) =
                futures::future::join_all(self.endpoint_controls.iter_mut().map(async |control| {
                    let endpoint = control
                        .disconnect()
                        .await
                        .context("failed to disconnect endpoint")?;
                    if let Some(endpoint) = endpoint {
                        if let Err(err) = endpoint.set_data_path_to_guest_vf(false).await {
                            tracing::error!(
                                vtl2_vfid,
                                err = err.as_ref() as &dyn std::error::Error,
                                "Failed to force data path to synthetic"
                            );
                        }
                        control
                            .connect(endpoint)
                            .context("failed to reconnect endpoint")?;
                    }
                    Ok::<(), anyhow::Error>(())
                }))
                .instrument(tracing::info_span!(
                    "forcing datapath to synthetic",
                    vtl2_vfid
                ))
                .await
                .into_iter()
                .collect::<anyhow::Result<Vec<_>, _>>()
            {
                tracing::error!(
                    vtl2_vfid,
                    err = err.as_ref() as &dyn std::error::Error,
                    "Failed forcing endpoint to switch data path"
                );
            }
            // Explicitly update save state mac filter settings in case of errors.
            for direction_to_vtl0 in &mut *self.save_state.direction_to_vtl0.lock() {
                *direction_to_vtl0 = Some(false);
            }
        }
        if let Err(err) = {
            let vpci_bus_control = if let Vtl0Bus::Present(current_bus) = bus_control {
                current_bus
            } else {
                let Vtl0Bus::Present(current_bus) = &self.vtl0_bus_control else {
                    unreachable!();
                };
                current_bus
            };

            self.revoke_vtl0_vf(vpci_bus_control).await
        } {
            tracing::error!(
                vtl2_vfid,
                err = err.as_ref() as &dyn std::error::Error,
                "Failed to revoke VTL0 VF"
            );
        }
    }

    fn notify_vtl0_vf_arrival(&mut self) {
        // Notify the network stack of an arrival, but don't wait for a response.
        for update in self.guest_state_notifications.iter() {
            drop(update.call(HclNetworkVFUpdateNotification::Update, ()));
        }
    }

    /// Tears down the current VTL2 device and disconnects all endpoints.
    ///
    /// On return, `self.mana_device` is `None` and packet capture controls have
    /// been dropped. If `keep_vf_alive` is true, the device handle is leaked on
    /// purpose so the host-side VF stays alive across servicing.
    pub async fn shutdown_vtl2_device(&mut self, keep_vf_alive: bool) {
        self.disconnect_all_endpoints().await;
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        if let Some(device) = self.mana_device.take() {
            let (result, device) = device
                .shutdown()
                .instrument(tracing::info_span!(
                    "shutdown vtl2 device",
                    vtl2_vfid,
                    keep_vf_alive
                ))
                .await;
            // Closing the VFIO device handle can take a long time. Leak the handle by
            // stashing it away.
            if keep_vf_alive {
                std::mem::forget(device);
            } else {
                if let Err(err) = result {
                    tracing::warn!(
                        vtl2_vfid,
                        error = err.as_ref() as &dyn std::error::Error,
                        "Destroying MANA device"
                    );
                    // Enable FLR to try to recover the device.
                    match vfio_set_device_reset_method(&self.vtl2_pci_id, PciDeviceResetMethod::Flr)
                    {
                        Ok(_) => {
                            tracing::info!(
                                vtl2_vfid,
                                "Attempt to reset device via FLR on next teardown."
                            );
                        }
                        Err(err) => {
                            tracing::warn!(
                                vtl2_vfid,
                                err = &err as &dyn std::error::Error,
                                "Failed to re-enable FLR"
                            );
                        }
                    }
                }
                drop(device);
            }
        }
    }

    /// Offers the visible VTL0 VF to the guest.
    ///
    /// Assumes the worker is not in shutdown.
    /// On return, `guest_state.offered_to_guest` is true only if the offer
    /// RPC succeeds; otherwise the worker state is left unchanged.
    async fn add_vtl0_vf(&mut self) {
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        if !self.guest_state.is_offered_to_guest().await
            && self.guest_state.vtl0_vfid().await.is_some()
        {
            if let Vtl0Bus::Present(vtl0_bus_control) = &self.vtl0_bus_control {
                match vtl0_bus_control
                    .offer_device()
                    .instrument(tracing::info_span!(
                        "adding VF to VTL0",
                        vtl2_vfid,
                        vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control)
                    ))
                    .await
                {
                    Ok(_) => {
                        *self.guest_state.offered_to_guest.lock().await = true;
                    }
                    Err(err) => {
                        tracing::error!(
                            vtl2_vfid,
                            vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                            err = err.as_ref() as &dyn std::error::Error,
                            "Failed to add VTL0 VF"
                        );
                    }
                }
            } else {
                tracing::info!(
                    vtl2_vfid,
                    %self.vtl0_bus_control,
                    "Ignoring VTL0 device request from guest"
                );
            }
        }
    }

    /// Applies a hide or unhide request for the VTL0 VF.
    ///
    /// Assumes the worker is not in shutdown.
    /// `vtl2_device_state` is passed explicitly because visibility changes are
    /// applied immediately only while the VTL2 device is present. If the device
    /// is missing or reconfiguring, the new bus state is staged on `self` and
    /// will take effect when VTL2 comes back. On return,
    /// `save_state.hidden_vtl0` always reflects the requested visibility.
    async fn hide_vtl0_vf(&mut self, rpc: Rpc<bool, ()>, vtl2_device_state: &Vtl2DeviceState) {
        rpc.handle(async |hide_vtl0| {
            tracing::info!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                hide_vtl0,
                "setting VTL0 VF hidden state"
            );
            *self.save_state.hidden_vtl0.lock() = Some(hide_vtl0);
            if hide_vtl0 {
                if !matches!(self.vtl0_bus_control, Vtl0Bus::HiddenPresent(_)) {
                    let old_bus_control =
                        std::mem::replace(&mut self.vtl0_bus_control, Vtl0Bus::HiddenNotPresent);
                    if matches!(old_bus_control, Vtl0Bus::Present(_)) {
                        if matches!(vtl2_device_state, Vtl2DeviceState::Present) {
                            *self.guest_state.vtl0_vfid.lock().await =
                                vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                            self.try_notify_guest_and_revoke_vtl0_vf(&old_bus_control)
                                .await;
                        }
                        let Vtl0Bus::Present(bus_control) = old_bus_control else {
                            unreachable!();
                        };
                        self.vtl0_bus_control = Vtl0Bus::HiddenPresent(bus_control);
                    }
                }
            } else {
                if matches!(self.vtl0_bus_control, Vtl0Bus::HiddenPresent(_)) {
                    let Vtl0Bus::HiddenPresent(bus_control) =
                        std::mem::replace(&mut self.vtl0_bus_control, Vtl0Bus::NotPresent)
                    else {
                        unreachable!();
                    };
                    self.vtl0_bus_control = Vtl0Bus::Present(bus_control);
                    if matches!(vtl2_device_state, Vtl2DeviceState::Present) {
                        *self.guest_state.vtl0_vfid.lock().await =
                            vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                        self.notify_vtl0_vf_arrival();
                    }
                } else if matches!(self.vtl0_bus_control, Vtl0Bus::HiddenNotPresent) {
                    self.vtl0_bus_control = Vtl0Bus::NotPresent;
                }
            }
        })
        .await
    }

    /// Revoke the VTL0 VF with a timeout in case VTL0 doesn't respond.
    ///
    /// Generally called when not in shutdown, but it's not assumed here.
    /// Takes an HclVpciBusControl pulled from `self.vtl0_bus_control` and
    /// revokes with a `MAX_WAIT_TIMEOUT` cancel context; empirical evidence
    /// shows that the vast majority of devices either revoke well within that
    /// timeout, or do not resolve at all.
    async fn revoke_vtl0_vf(&self, bus_control: &HclVpciBusControl) -> anyhow::Result<()> {
        let mut ctx = mesh::CancelContext::new().with_timeout(MAX_WAIT_TIMEOUT);
        ctx.until_cancelled(bus_control.revoke_device().instrument(
            tracing::info_span!("revoking vtl0 vf", vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control), vtl0_bus = %bus_control),
        ))
        .await
        .unwrap_or_else(|cr| Err(anyhow!("vtl0 revoke timed out: {cr}")))
    }

    /// Removes the VTL0 VF from the guest.
    ///
    /// Generally called when not in shutdown, but it's not assumed here.
    /// The guest-facing offer bit is cleared before the revoke RPC is issued so
    /// duplicate removals become no-ops. On return,
    /// `guest_state.offered_to_guest` is always false even if the RPC fails or
    /// times out.
    async fn remove_vtl0_vf(&mut self) {
        let vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        if self.guest_state.is_offered_to_guest().await {
            *self.guest_state.offered_to_guest.lock().await = false;
            if let Vtl0Bus::Present(vtl0_bus_control) = &self.vtl0_bus_control {
                match self.revoke_vtl0_vf(vtl0_bus_control).await {
                    Ok(_) => (),
                    Err(err) => {
                        tracing::error!(
                            vtl2_vfid,
                            vtl0_vfid,
                            err = err.as_ref() as &dyn std::error::Error,
                            "Failed to remove VTL0 VF"
                        );
                    }
                }
            }
        }
    }

    /// Updates which VTL0 VF, if any, is associated with this worker.
    ///
    /// Assumes the worker is not in shutdown.
    /// When `vtl2_device_state` is `Present`, the guest-visible VF id and
    /// arrival/removal notifications are updated immediately. Otherwise the bus
    /// change is recorded on `self.vtl0_bus_control` and the guest-facing state
    /// remains cleared until the VTL2 device is started again.
    async fn update_vtl0_vf(
        &mut self,
        rpc: Rpc<Option<HclVpciBusControl>, ()>,
        vtl2_device_state: &Vtl2DeviceState,
    ) {
        rpc.handle(async |bus_control| {
            let is_present = matches!(
                self.vtl0_bus_control,
                Vtl0Bus::Present(_) | Vtl0Bus::HiddenPresent(_)
            );
            assert!(is_present != bus_control.is_some());
            tracing::info!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                present = bus_control.is_some(),
                "VTL0 VF device change"
            );
            if matches!(&self.vtl0_bus_control, Vtl0Bus::HiddenNotPresent) {
                self.vtl0_bus_control = Vtl0Bus::HiddenPresent(bus_control.unwrap())
            } else if matches!(&self.vtl0_bus_control, Vtl0Bus::HiddenPresent(_)) {
                self.vtl0_bus_control = Vtl0Bus::HiddenNotPresent;
            } else if matches!(vtl2_device_state, Vtl2DeviceState::Present) {
                let bus_control = bus_control
                    .map(Vtl0Bus::Present)
                    .unwrap_or(Vtl0Bus::NotPresent);
                *self.guest_state.vtl0_vfid.lock().await = vtl0_vfid_from_bus_control(&bus_control);
                let old_bus_control = std::mem::replace(&mut self.vtl0_bus_control, bus_control);
                match self.vtl0_bus_control {
                    Vtl0Bus::Present(_) => self.notify_vtl0_vf_arrival(),
                    Vtl0Bus::NotPresent => {
                        self.try_notify_guest_and_revoke_vtl0_vf(&old_bus_control)
                            .await
                    }
                    _ => unreachable!(),
                }
            } else {
                // When the VTL2 device is restored, the VTL0 update will be applied.
                assert_eq!(*self.guest_state.offered_to_guest.lock().await, false);
                assert!(self.guest_state.vtl0_vfid.lock().await.is_none());
                self.vtl0_bus_control = bus_control
                    .map(Vtl0Bus::Present)
                    .unwrap_or(Vtl0Bus::NotPresent);
            }
        })
        .await
    }

    async fn disconnect_all_endpoints(&mut self) {
        let num_endpoints = self.endpoint_controls.len();
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);

        futures::future::join_all(self.endpoint_controls.iter_mut().map(async |control| {
            match control.disconnect().await {
                Ok(Some(mut endpoint)) => {
                    tracing::info!(vtl2_vfid, "Network endpoint disconnected");
                    endpoint.stop().await;
                }
                Ok(None) => {
                    tracing::info!(
                        vtl2_vfid,
                        "network endpoint disconnect processed; no endpoint connected"
                    );
                }
                Err(err) => {
                    tracing::error!(
                        vtl2_vfid,
                        err = err.as_ref() as &dyn std::error::Error,
                        "Failed to disconnect endpoint"
                    );
                }
            }
        }))
        .instrument(tracing::info_span!(
            "disconnecting all endpoints",
            vtl2_vfid,
            num_endpoints
        ))
        .await;

        self.pkt_capture_controls = None;
    }

    async fn update_vtl2_device_bind_state(&self, is_bound: bool) -> anyhow::Result<()> {
        self.vtl2_bus_control
            .update_vtl2_device_bind_state(is_bound)
            .instrument(tracing::info_span!(
                "update vtl2 device bind state",
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                is_bound,
            ))
            .await
    }

    /// Attempts to recreate the VTL2 MANA device and reconnect its endpoints.
    ///
    /// On success, `self.mana_device` is repopulated, endpoint controls are
    /// connected again, and a visible VTL0 VF is re-announced to the guest.
    /// The caller remains responsible for updating the loop-local
    /// `Vtl2DeviceState`.
    async fn startup_vtl2_device(&mut self, update_vtl2_device_bind_state: bool) -> bool {
        // Each async call within this function handles its own tracing.
        let mut vtl2_device_present = false;
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        let vtl2_instance_id = self.vtl2_bus_control.instance_id();
        let device_bound = match create_mana_device(
            &self.driver_source,
            &self.vtl2_pci_id,
            &vtl2_instance_id,
            self.vp_count,
            self.max_sub_channels,
            KeepAliveConfig::Disabled,
            self.dma_clients.clone(),
            None, // No ManaSavedState
        )
        .await
        {
            Ok(mut device) => {
                // Subscribe to HWC reset request events before starting notification task
                self.vf_reset_request_receiver = Some(device.subscribe_vf_reset_request().await);
                // Resubscribe to notifications from the MANA device.
                device.start_notification_task(&self.driver_source).await;

                self.mana_device = Some(device);
                self.connect_endpoints().await.is_ok()
            }
            Err(err) => {
                tracing::error!(
                    vtl2_vfid,
                    err = err.as_ref() as &dyn std::error::Error,
                    "Failed to create MANA device"
                );
                false
            }
        };

        if update_vtl2_device_bind_state {
            if let Err(err) = self.update_vtl2_device_bind_state(device_bound).await {
                tracing::error!(
                    vtl2_vfid,
                    err = err.as_ref() as &dyn std::error::Error,
                    "Failed to report new binding state to host"
                );
            }
        }

        if device_bound {
            vtl2_device_present = true;
            if matches!(&self.vtl0_bus_control, Vtl0Bus::Present(_)) {
                *self.guest_state.vtl0_vfid.lock().await =
                    vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                self.notify_vtl0_vf_arrival();
            }
        }

        vtl2_device_present
    }

    /// Creates a stored state package to restore guest connectivity later
    ///
    /// Assumes the worker is in shutdown.
    /// This code will capture the state of the MANA device to simplify
    /// re-initialization for the guest; on failure, it attempts to set
    /// the MANA device to recover gracefully.
    async fn save_mana_device_state(&mut self, rpc: Rpc<(), VfManagerSaveResult>) {
        assert!(self.is_shutdown_active);
        drop(self.messages.take().unwrap());
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        rpc.handle(async |_| {
            self.disconnect_all_endpoints().await;

            if let Some(device) = self.mana_device.take() {
                let (saved_state, device) = device
                    .save()
                    .instrument(tracing::info_span!(
                        "saving mana device state",
                        vtl2_vfid,
                        vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                    ))
                    .await;

                match saved_state {
                    Ok(saved_state) => {
                        // Closing the VFIO device handle can take a long time.
                        // Leak the handle by stashing it away.
                        std::mem::forget(device);
                        VfManagerSaveResult::Saved(ManaSavedState {
                            mana_device: saved_state,
                            pci_id: self.vtl2_pci_id.clone(),
                        })
                    }
                    Err(err) => {
                        tracing::error!(
                            vtl2_vfid,
                            vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                            error = err.as_ref() as &dyn std::error::Error,
                            "Failed while saving MANA device state"
                        );
                        // Enable FLR to try to recover the device.
                        match vfio_set_device_reset_method(
                            &self.vtl2_pci_id,
                            PciDeviceResetMethod::Flr,
                        ) {
                            Ok(_) => {
                                tracing::info!(
                                    vtl2_vfid,
                                    "Attempt to reset device via FLR on next teardown."
                                );
                            }
                            Err(err) => {
                                tracing::warn!(
                                    vtl2_vfid,
                                    err = &err as &dyn std::error::Error,
                                    "Failed to re-enable FLR"
                                );
                            }
                        }
                        drop(device);
                        VfManagerSaveResult::SaveFailed
                    }
                }
            } else {
                tracing::warn!(vtl2_vfid, "no MANA device present when saving state");
                VfManagerSaveResult::DeviceMissing
            }
        })
        .await
    }

    /// Begins recovery after the MANA device requests VF reconfiguration.
    ///
    /// This clears the guest-visible VTL0 state, revokes any offered VF, and
    /// tears down the current VTL2 device. On return, `vtl2_device_state` is
    /// `Reconfiguring` and the returned backoff schedules the first restart
    /// attempt.
    async fn reconfigure_vf(
        &mut self,
        vtl2_device_state: &mut Vtl2DeviceState,
    ) -> Option<VfReconfigBackoff> {
        // Remove VTL0 VF if present
        *self.guest_state.vtl0_vfid.lock().await = None;
        if self.guest_state.is_offered_to_guest().await {
            tracing::warn!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                "VTL0 VF being removed as a result of VF Reconfiguration."
            );
            self.try_notify_guest_and_revoke_vtl0_vf(&Vtl0Bus::NotPresent)
                .await;
        }

        // Don't 'keep alive'. VTL2 is reconfigured when in a bad state.
        self.shutdown_vtl2_device(false).await;

        // Start the VTL2 device and resubscribe to notifications.
        // After sending the VF Reconfiguration notification, the SoC may need time to recover.
        // Keep retrying with backoff until the device successfully restarts.
        *vtl2_device_state = Vtl2DeviceState::Reconfiguring;
        Some(VfReconfigBackoff {
            deadline: Instant::now().saturating_add(RECONFIG_INITIAL_SLEEP),
            sleep: RECONFIG_INITIAL_SLEEP,
            attempts: 0,
        })
    }

    /// Attempts to restart the VTL2 device while the worker is reconfiguring the VF.
    ///
    /// Assumes the worker is not in shutdown.
    /// On success, returns `Ok(None)` and sets device state to `Present` when the device has
    /// started up. If retries have run out, sets device state to `Missing` and returns an
    /// error; otherwise, `Ok(Some(backoff))` with updated retry timing.
    async fn reconfigure_vf_restart(
        &mut self,
        vtl2_device_state: &mut Vtl2DeviceState,
        mut backoff: VfReconfigBackoff,
    ) -> anyhow::Result<Option<VfReconfigBackoff>> {
        backoff.attempts += 1;
        let update_vtl2_device_bind_state = false;
        let restarted = self
            .startup_vtl2_device(update_vtl2_device_bind_state)
            .await;
        if restarted {
            tracing::info!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                attempts = backoff.attempts,
                "VTL2 device restarted after VF reconfiguration"
            );
            *vtl2_device_state = Vtl2DeviceState::Present;
            Ok(None)
        } else {
            if backoff.attempts >= RECONFIG_MAX_ATTEMPTS {
                tracing::error!(
                    vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                    attempts = backoff.attempts,
                    "VTL2 device restart not ready after VF reconfiguration"
                );
                // Stop further attempts.
                *vtl2_device_state = Vtl2DeviceState::Missing;
                anyhow::bail!("vtl2 device not ready")
            }

            if backoff.attempts == 1 || backoff.attempts.is_multiple_of(10) {
                tracing::warn!(
                    vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                    attempts = backoff.attempts,
                    sleep_ms = backoff.sleep.as_millis(),
                    "VTL2 device restart not ready after VF reconfiguration; retrying"
                );
            }

            backoff.sleep = std::cmp::min(RECONFIG_MAX_SLEEP, backoff.sleep.saturating_mul(2));
            backoff.deadline = Instant::now().saturating_add(backoff.sleep);
            Ok(Some(backoff))
        }
    }

    /// Handles a VTL2 arrival event after the device was previously missing.
    ///
    /// Assumes the run loop has already ruled out shutdown and any pending
    /// reconfiguration timer. On return, `vtl2_device_state` is set to
    /// `Present` only if startup succeeds; otherwise the worker stays in its
    /// prior degraded state and waits for another recovery signal.
    async fn mana_device_arrived(&mut self, vtl2_device_state: &mut Vtl2DeviceState) {
        let mut ctx = mesh::CancelContext::new().with_timeout(std::time::Duration::from_secs(1));
        // Ignore error here for waiting for the PCI path and continue to create the MANA device.
        if ctx
            .until_cancelled(wait_for_pci_path(&self.vtl2_pci_id))
            .await
            .is_err()
        {
            let pci_path = Path::new("/sys/bus/pci/devices").join(&self.vtl2_pci_id);
            tracing::error!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                ?pci_path,
                "Timed out waiting for MANA PCI path"
            );
        }

        let update_vtl2_device_bind_state = true;
        if self
            .startup_vtl2_device(update_vtl2_device_bind_state)
            .await
        {
            *vtl2_device_state = Vtl2DeviceState::Present;
        }
    }

    /// Handles VTL2 device removal from the vPCI bus.
    ///
    /// This function is assumed to not be running during shutdown.
    /// This always clears the guest-visible VTL0 identity, tears down the VTL2
    /// device, and cancels any outstanding reconfiguration retry. On return,
    /// `vtl2_device_state` is `Missing` and the host has been told the device is
    /// no longer bound.
    async fn mana_device_removed(
        &mut self,
        vtl2_device_state: &mut Vtl2DeviceState,
        vf_reconfig_backoff: &mut Option<VfReconfigBackoff>,
    ) {
        *self.guest_state.vtl0_vfid.lock().await = None;
        if self.guest_state.is_offered_to_guest().await {
            tracing::info!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                "VTL0 VF being removed as a result of VTL2 VF revoke."
            );
            self.try_notify_guest_and_revoke_vtl0_vf(&Vtl0Bus::NotPresent)
                .await;
        }

        self.shutdown_vtl2_device(false).await;
        *vtl2_device_state = Vtl2DeviceState::Missing;
        // If the device is being removed, remove outstanding vf reconfiguration.
        *vf_reconfig_backoff = None;

        if let Err(err) = self.update_vtl2_device_bind_state(false).await {
            tracing::error!(
                vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control),
                err = err.as_ref() as &dyn std::error::Error,
                "Failed to report new binding state to host"
            );
        }
    }

    pub async fn run(&mut self) {
        #[derive(Debug)]
        enum NextWorkItem {
            Continue,
            ManagerMessage(HclNetworkVfManagerMessage),
            ManaDeviceArrived,
            ManaDeviceRemoved,
            VfReconfig,
            VfReconfigRestart,
            ExitWorker,
        }

        let mut vtl2_device_state = Vtl2DeviceState::Present;
        let mut vf_reconfig_backoff: Option<VfReconfigBackoff> = None;
        let vtl2_vfid = vtl2_vfid_from_bus_control(&self.vtl2_bus_control);
        loop {
            let next_work_item = {
                let next_message = self
                    .messages
                    .as_mut()
                    .unwrap()
                    .map(NextWorkItem::ManagerMessage)
                    .chain(iter([NextWorkItem::ExitWorker]));
                // VPCI bus events determine when the MANA device has been Removed.
                let device_change = self.vtl2_bus_control.notifier().map(|device| match device {
                    VpciBusEvent::DeviceEnumerated => {
                        tracing::info!(vtl2_vfid, "MANA device enumerated, waiting for uevent.");
                        NextWorkItem::Continue
                    }
                    VpciBusEvent::PrepareForRemoval => NextWorkItem::ManaDeviceRemoved,
                });
                // UEVENT notifications determine when the MANA device has Arrived.
                let device_arrival = (&mut self.uevent_handler).map(|notification| {
                    let UeventNotification {
                        device_path,
                        action,
                    } = notification;
                    // Prior behavior treats any uevent with a valid device path as an arrival, as long
                    // as the VTL2 device is currently missing. Otherwise, uevents are silently ignored.
                    // It would be more correct to check that the uevent action is 'add'.
                    let exists = Path::new(&device_path).exists();
                    match (vtl2_device_state, exists) {
                        (Vtl2DeviceState::Missing, true) => NextWorkItem::ManaDeviceArrived,
                        (state, false) => {
                            // Tracing to diagnose add that is not acted on due to missing device.
                            if action == UeventAction::Add {
                                 tracelimit::warn_ratelimited!(?state, ?action, exists, %device_path, "uevent received");
                            }
                            NextWorkItem::Continue
                        }
                        _ => NextWorkItem::Continue,
                    }
                });

                let vf_reset_request = self
                    .vf_reset_request_receiver
                    .as_mut()
                    .unwrap()
                    .map(|()| NextWorkItem::VfReconfig);
                let reconfig_restart_deadline = vf_reconfig_backoff.map(|backoff| backoff.deadline);
                let wait_for_reconfig = futures::stream::once(async {
                    match reconfig_restart_deadline {
                        Some(deadline) => {
                            let mut timer = PolledTimer::new(&self.driver_source.simple());
                            timer.sleep_until(deadline).await;
                        }
                        None => pending().await,
                    }
                });
                let vf_restart_tick_wait = std::pin::pin!(wait_for_reconfig);
                let vf_restart_tick =
                    vf_restart_tick_wait.map(|()| NextWorkItem::VfReconfigRestart);

                (
                    next_message,
                    device_change,
                    device_arrival,
                    vf_reset_request,
                    vf_restart_tick,
                )
                    .merge()
                    .next()
                    .await
                    .unwrap()
            };

            match next_work_item {
                NextWorkItem::Continue => continue,
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::Inspect(deferred)) => {
                    deferred.inspect(&self)
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::AddGuestVFManager(
                    rpc,
                )) => {
                    rpc.handle(async |send_update| {
                        self.guest_state_notifications.push(send_update);
                        self.guest_state.clone()
                    })
                    .instrument(tracing::info_span!("add guest vf manager", vtl2_vfid))
                    .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::PacketCapture(rpc)) => {
                    rpc.handle_failable(async |params| self.handle_packet_capture(params).await)
                        .instrument(tracing::info_span!("packet capture", vtl2_vfid))
                        .await
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::AddVtl0VF) => {
                    if self.is_shutdown_active {
                        continue;
                    }

                    self.add_vtl0_vf()
                        .instrument(tracing::info_span!("add vtl0 vf", vtl2_vfid))
                        .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::RemoveVtl0VF) => {
                    if self.is_shutdown_active {
                        continue;
                    }

                    let vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                    self.remove_vtl0_vf()
                        .instrument(tracing::info_span!("remove vtl0 vf", vtl2_vfid, vtl0_vfid))
                        .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::UpdateVtl0VF(rpc)) => {
                    if self.is_shutdown_active {
                        rpc.complete(());
                        continue;
                    }

                    let vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                    self.update_vtl0_vf(rpc, &vtl2_device_state)
                        .instrument(tracing::info_span!("update vtl0 vf", vtl2_vfid, vtl0_vfid))
                        .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::HideVtl0VF(rpc)) => {
                    if self.is_shutdown_active {
                        rpc.complete(());
                        continue;
                    }

                    let vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                    self.hide_vtl0_vf(rpc, &vtl2_device_state)
                        .instrument(tracing::info_span!("hide vtl0 vf", vtl2_vfid, vtl0_vfid))
                        .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::SaveState(rpc)) => {
                    self.save_mana_device_state(rpc)
                        .instrument(tracing::info_span!("save mana state", vtl2_vfid))
                        .await;
                    // Exit worker thread.
                    return;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::ShutdownBegin(
                    remove_vtl0_vf,
                )) => {
                    let vtl0_vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                    tracing::info!(
                        vtl2_vfid,
                        vtl0_vfid,
                        remove_vtl0_vf,
                        "beginning VTL2 device shutdown"
                    );
                    if remove_vtl0_vf {
                        self.remove_vtl0_vf()
                            .instrument(tracing::info_span!(
                                "remove vtl0 vf for shutdown",
                                vtl2_vfid,
                                vtl0_vfid
                            ))
                            .await;
                    }
                    self.is_shutdown_active = true;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::ShutdownComplete(rpc)) => {
                    tracing::info!(vtl2_vfid, "shutting down VTL2 device");
                    assert!(self.is_shutdown_active);
                    drop(self.messages.take().unwrap());
                    rpc.handle(async |keep_vf_alive| {
                        self.shutdown_vtl2_device(keep_vf_alive).await;
                    })
                    .instrument(tracing::info_span!(
                        "completing VTL2 device shutdown",
                        vtl2_vfid
                    ))
                    .await;
                    // Exit worker thread.
                    return;
                }
                NextWorkItem::VfReconfig => {
                    if self.is_shutdown_active
                        || matches!(vtl2_device_state, Vtl2DeviceState::Missing)
                    {
                        tracing::debug!(
                            vtl2_vfid,
                            is_shutdown_active = self.is_shutdown_active,
                            vtl2_device_state_missing =
                                matches!(vtl2_device_state, Vtl2DeviceState::Missing),
                            "Skipping VF reconfiguration during shutdown or when device is missing"
                        );
                        continue;
                    }

                    vf_reconfig_backoff = self
                        .reconfigure_vf(&mut vtl2_device_state)
                        .instrument(tracing::info_span!(
                            "VTL2 VF reconfiguration requested",
                            vtl2_vfid
                        ))
                        .await;
                }
                NextWorkItem::VfReconfigRestart => {
                    let Some(backoff) = vf_reconfig_backoff else {
                        tracing::debug!(
                            vtl2_vfid,
                            "VF reconfiguration restart without backoff state"
                        );
                        continue;
                    };

                    if self.is_shutdown_active {
                        tracing::warn!(vtl2_vfid, "VF reconfiguration restart during shutdown");
                        vf_reconfig_backoff = None;
                        continue;
                    }

                    vf_reconfig_backoff = self
                        .reconfigure_vf_restart(&mut vtl2_device_state, backoff)
                        .instrument(tracing::info_span!("VF reconfiguration restart", vtl2_vfid))
                        .await
                        .unwrap_or(None);
                }
                NextWorkItem::ManaDeviceArrived => {
                    if self.is_shutdown_active {
                        tracing::error!(vtl2_vfid, "MANA device arrival during shutdown");
                        continue;
                    }
                    assert!(
                        vf_reconfig_backoff.is_none(),
                        "device arrival should only occur after device removal and not vf reconfiguration"
                    );
                    self.mana_device_arrived(&mut vtl2_device_state)
                        .instrument(tracing::info_span!("VTL2 VF arrived", vtl2_vfid))
                        .await;
                }
                NextWorkItem::ManaDeviceRemoved => {
                    if self.is_shutdown_active {
                        tracing::warn!(vtl2_vfid, "MANA device removal during shutdown");
                        continue;
                    }
                    self.mana_device_removed(&mut vtl2_device_state, &mut vf_reconfig_backoff)
                        .instrument(tracing::info_span!("VTL2 VF removal", vtl2_vfid))
                        .await;
                }
                NextWorkItem::ExitWorker => {
                    drop(self.messages.take().unwrap());
                    tracing::info!(vtl2_vfid, "Worker exiting");
                    return;
                }
            }
        }
    }

    async fn handle_packet_capture(
        &mut self,
        params: PacketCaptureParams<Socket>,
    ) -> anyhow::Result<PacketCaptureParams<Socket>> {
        let Some(pkt_capture_controls) = &self.pkt_capture_controls else {
            anyhow::bail!("Packet capture controls have not been setup")
        };

        let mut params = params;
        for control in pkt_capture_controls.iter() {
            params = control.packet_capture(params).await?;
        }
        Ok(params)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UeventAction {
    Add,
    Remove,
    Rescan,
}

struct UeventNotification {
    device_path: String,
    action: UeventAction,
}

struct HclNetworkVfManagerUeventHandler {
    uevent_receiver: mesh::Receiver<UeventNotification>,
    _callback_handle: uevent::CallbackHandle,
}

impl HclNetworkVfManagerUeventHandler {
    pub async fn new(uevent_listener: &UeventListener, instance_id: Guid) -> Self {
        let pci_id = format!("pci{0:04x}:00/{0:04x}:00:00.0", instance_id.data2);
        let device_path = format!("/devices/platform/bus/bus:vmbus/{}/{}", instance_id, pci_id);
        // File system device path is not the same as the uevent path.
        let fs_dev_path = format!("/sys/bus/vmbus/devices/{}/{}", instance_id, pci_id);
        let (tx, rx) = mesh::channel();
        let callback = move |notification: uevent::Notification<'_>| {
            let uevent::Notification::Event(uevent) = notification;
            let action = uevent.get("ACTION").unwrap_or("unknown");
            let dev_path = uevent.get("DEVPATH").unwrap_or("unknown");
            if device_path == dev_path {
                let uevent_action = match action {
                    "add" => Some(UeventAction::Add),
                    "remove" => Some(UeventAction::Remove),
                    _ => None,
                };
                if let Some(uevent_action) = uevent_action {
                    tx.send(UeventNotification {
                        device_path: fs_dev_path.clone(),
                        action: uevent_action,
                    });
                }
            } else if uevent.get("RESCAN") == Some("true") {
                tx.send(UeventNotification {
                    device_path: fs_dev_path.clone(),
                    action: UeventAction::Rescan,
                });
            }
        };
        let callback_handle = uevent_listener.add_custom_callback(callback).await;
        Self {
            uevent_receiver: rx,
            _callback_handle: callback_handle,
        }
    }
}

impl futures::Stream for HclNetworkVfManagerUeventHandler {
    type Item = UeventNotification;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Poll::Ready(ready!(this.uevent_receiver.poll_recv(cx)).ok())
    }
}

impl futures::stream::FusedStream for HclNetworkVfManagerUeventHandler {
    fn is_terminated(&self) -> bool {
        self.uevent_receiver.is_terminated()
    }
}

pub struct HclNetworkVFManagerEndpointInfo {
    pub adapter_index: u32,
    pub mac_address: MacAddress,
    pub endpoint: Box<DisconnectableEndpoint>,
}

#[derive(Inspect)]
struct HclNetworkVFManagerSharedState {
    #[inspect(flatten, send = "HclNetworkVfManagerMessage::Inspect")]
    worker_channel: mesh::Sender<HclNetworkVfManagerMessage>,
}

enum HclNetworkVFUpdateNotification {
    Update(Rpc<(), ()>),
}

#[derive(Inspect)]
pub struct HclNetworkVFManager {
    #[inspect(flatten)]
    shared_state: Arc<HclNetworkVFManagerSharedState>,
    #[inspect(skip)]
    _task: Task<()>,
}

#[derive(Protobuf, Clone, Debug)]
#[mesh(package = "network_adapter_index")]
pub struct NetworkAdapterIndexSavedState {
    #[mesh(1)]
    mac_address: [u8; 6],
    #[mesh(2)]
    adapter_index: u32,
}

/// Provides for serializing the network adapter index generation across multiple
/// network VF managers.
#[derive(Clone)]
pub struct NetworkAdapterIndex {
    state: Arc<parking_lot::Mutex<NetworkAdapterIndexState>>,
}

struct NetworkAdapterIndexState {
    /// The next adapter index to issue.
    index: u32,
    mac_address_to_index: HashMap<MacAddress, u32>,
}

impl NetworkAdapterIndexState {
    fn increment_index(&mut self) {
        self.index = self.index.wrapping_add(1);
        if self.index == 0 {
            // Skip 0 as it's treated as invalid by various guest code.
            self.index = 1;
        }
    }
}

impl NetworkAdapterIndex {
    pub fn new(initial_value: Option<u32>) -> Self {
        Self {
            state: Arc::new(parking_lot::Mutex::new(NetworkAdapterIndexState {
                // Adapter index is used to generate the serial number for the
                // guest and there are various guest code that treat a serial number
                // of '0' as invalid. Start at 1 to avoid that.
                index: initial_value.unwrap_or(1),
                mac_address_to_index: HashMap::default(),
            })),
        }
    }

    /// Returns the next adapter index and increments the internal counter.
    pub fn next(&self, mac_address: &MacAddress) -> u32 {
        let mut state = self.state.lock();
        if let Some(&index) = state.mac_address_to_index.get(mac_address) {
            return index;
        }

        // The choice to limit the number of adapter index to 1024 is a bit arbitrary here,
        // but the intent is to prevent unintended use here to support a very large number
        // of network interfaces, which requires broader rethinking on how the adapters
        // are managed.
        assert!(
            state.mac_address_to_index.len() < 1024_usize,
            "network adapter index capacity exhausted; maximum supported network adapters is 1024"
        );

        // Find the next index that isn't already used by another MAC address
        while state
            .mac_address_to_index
            .values()
            .any(|&v| v == state.index)
        {
            state.increment_index();
        }

        let assigned = state.index;
        state.increment_index();
        state.mac_address_to_index.insert(*mac_address, assigned);
        assigned
    }

    /// Removes the adapter index associated with the given MAC address.
    pub fn remove(&self, mac_address: &MacAddress) {
        self.state.lock().mac_address_to_index.remove(mac_address);
    }

    /// Returns the saved state of the network adapter index mapping.
    pub fn save(&self) -> Option<Vec<NetworkAdapterIndexSavedState>> {
        let state = self.state.lock();
        Some(
            state
                .mac_address_to_index
                .iter()
                .map(
                    |(&mac_address, &adapter_index)| NetworkAdapterIndexSavedState {
                        mac_address: mac_address.to_bytes(),
                        adapter_index,
                    },
                )
                .collect(),
        )
    }

    /// Restores the network adapter index mapping from the saved state.
    pub fn restore(saved_states: Option<Vec<NetworkAdapterIndexSavedState>>) -> Self {
        // When restoring from older saved state that lacks adapter index data,
        // start at index 0 to preserve backward compatibility.
        let restored_state = if saved_states.is_none() {
            Self::new(Some(0))
        } else {
            Self::new(None)
        };

        if let Some(saved_states) = saved_states {
            let mut state = restored_state.state.lock();
            for saved_state in saved_states {
                let mac_address = MacAddress::new(saved_state.mac_address);
                state
                    .mac_address_to_index
                    .insert(mac_address, saved_state.adapter_index);
                if saved_state.adapter_index >= state.index {
                    state.index = saved_state.adapter_index;
                    state.increment_index();
                }
            }
        }

        restored_state
    }
}

impl HclNetworkVFManager {
    pub async fn new(
        vtl2_vf_instance_id: Guid,
        vtl2_pci_id: String,
        vtl0_vf_instance_id: Option<Guid>,
        get: GuestEmulationTransportClient,
        driver_source: &VmTaskDriverSource,
        uevent_listener: &UeventListener,
        vp_count: u32,
        max_sub_channels: u16,
        netvsp_state: &Option<Vec<SavedState>>,
        dma_mode: GuestDmaMode,
        keepalive_mode: KeepAliveConfig,
        dma_clients: VfioDmaClients,
        mana_state: Option<&ManaSavedState>,
        network_adapter_index: NetworkAdapterIndex,
    ) -> anyhow::Result<(
        Self,
        Vec<HclNetworkVFManagerEndpointInfo>,
        RuntimeSavedState,
    )> {
        let device = create_mana_device(
            driver_source,
            &vtl2_pci_id,
            &vtl2_vf_instance_id,
            vp_count,
            max_sub_channels,
            keepalive_mode.clone(),
            dma_clients.clone(),
            mana_state,
        )
        .await?;
        let (mut endpoints, endpoint_controls): (Vec<_>, Vec<_>) = (0..device.num_vports())
            .map(|_| {
                let (endpoint, endpoint_control) = DisconnectableEndpoint::new();
                (Box::new(endpoint), endpoint_control)
            })
            .collect::<Vec<(Box<DisconnectableEndpoint>, DisconnectableEndpointControl)>>()
            .into_iter()
            .unzip();

        let vtl2_bus_control = HclVpciBusControl::new(get.clone(), vtl2_vf_instance_id).await?;
        let vtl0_bus_control = if let Some(vtl0_vf_instance_id) = vtl0_vf_instance_id {
            Some(HclVpciBusControl::new(get, vtl0_vf_instance_id).await?)
        } else {
            None
        };
        let uevent_handler =
            HclNetworkVfManagerUeventHandler::new(uevent_listener, vtl2_vf_instance_id).await;

        // Create save state, restoring previous values if they exist.
        let runtime_save_state = {
            let restored_state = if let Some(save_state) = netvsp_state {
                let mut restored_state = None;
                for state in save_state {
                    if state.instance_id == vtl2_vf_instance_id {
                        restored_state = Some(state.into());
                        break;
                    }
                }
                restored_state
            } else {
                None
            };
            restored_state.unwrap_or(RuntimeSavedState::new(vtl2_vf_instance_id))
        };

        let (mut worker, worker_channel) = HclNetworkVFManagerWorker::new(
            device,
            runtime_save_state.clone(),
            vtl2_pci_id,
            vtl2_bus_control,
            vtl0_bus_control,
            uevent_handler,
            driver_source,
            endpoint_controls,
            vp_count,
            max_sub_channels,
            dma_mode,
            dma_clients,
            network_adapter_index,
        );

        // Queue new endpoints.
        let endpoint_info = worker.connect_endpoints().await?;
        // The proxy endpoints are not yet in use, so run them here to switch to the queued endpoints.
        // N.B Endpoint should not return any other action type other than `RestartRequired`
        //     at this time because the notification task hasn't been started yet.
        futures::future::join_all(endpoints.iter_mut().map(async |endpoint| {
            let message = endpoint.wait_for_endpoint_action().await;
            assert_eq!(message, net_backend::EndpointAction::RestartRequired);
        }))
        .await;

        // Now that the endpoints are connected, start the device notification task that will
        // listen for and relay endpoint actions.
        let device = worker.mana_device.as_mut().unwrap();
        // Subscribe to HWC reset request events before starting notification task
        worker.vf_reset_request_receiver = Some(device.subscribe_vf_reset_request().await);
        device.start_notification_task(driver_source).await;
        let endpoints = endpoints
            .into_iter()
            .zip(endpoint_info)
            .map(
                |(endpoint, (mac_address, adapter_index))| HclNetworkVFManagerEndpointInfo {
                    adapter_index,
                    mac_address,
                    endpoint,
                },
            )
            .collect();

        let task = driver_source
            .simple()
            .spawn("MANA worker task", async move { worker.run().await });

        let shared_state = Arc::new(HclNetworkVFManagerSharedState { worker_channel });
        Ok((
            Self {
                shared_state,
                _task: task,
            },
            endpoints,
            runtime_save_state,
        ))
    }

    pub async fn save(&self) -> Option<ManaSavedState> {
        let save_state = self
            .shared_state
            .worker_channel
            .call(HclNetworkVfManagerMessage::SaveState, ())
            .await;

        match save_state {
            Ok(VfManagerSaveResult::Saved(state)) => Some(state),
            Ok(VfManagerSaveResult::DeviceMissing) => {
                tracing::warn!("MANA device missing when saving state");
                None
            }
            Ok(VfManagerSaveResult::SaveFailed) => {
                tracing::error!("MANA device present but save failed");
                None
            }
            Err(err) => {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    "RPC failure when saving VF Manager state"
                );
                None
            }
        }
    }

    pub async fn packet_capture(
        &self,
        params: PacketCaptureParams<Socket>,
    ) -> anyhow::Result<PacketCaptureParams<Socket>> {
        self.shared_state
            .worker_channel
            .call(HclNetworkVfManagerMessage::PacketCapture, params)
            .await?
            .map_err(anyhow::Error::from)
    }

    pub async fn create_function<F, R>(
        self: Arc<Self>,
        set_vport_ready_and_get_vf_state: F,
    ) -> anyhow::Result<Box<dyn netvsp::VirtualFunction>>
    where
        F: Fn(bool) -> R + Sync + Send + 'static,
        R: Future<Output = bool> + Send,
    {
        let (tx_update, rx_update) = mesh::channel();
        let guest_state = self
            .shared_state
            .worker_channel
            .call(HclNetworkVfManagerMessage::AddGuestVFManager, tx_update)
            .await
            .map_err(anyhow::Error::from)?;
        Ok(Box::new(HclNetworkVFManagerInstance::new(
            guest_state,
            self.shared_state.clone(),
            rx_update,
            set_vport_ready_and_get_vf_state,
        )))
    }

    pub async fn update_vtl0_instance_id(
        &self,
        vtl0_vf_instance_id: Option<Guid>,
        get: GuestEmulationTransportClient,
    ) -> anyhow::Result<()> {
        let vtl0_bus_control = if let Some(vtl0_vf_instance_id) = vtl0_vf_instance_id {
            Some(HclVpciBusControl::new(get, vtl0_vf_instance_id).await?)
        } else {
            None
        };
        self.shared_state
            .worker_channel
            .call(HclNetworkVfManagerMessage::UpdateVtl0VF, vtl0_bus_control)
            .await
            .map_err(anyhow::Error::from)
    }

    pub async fn hide_vtl0_instance(&self, hide_vtl0: bool) -> anyhow::Result<()> {
        self.shared_state
            .worker_channel
            .call(HclNetworkVfManagerMessage::HideVtl0VF, hide_vtl0)
            .await
            .map_err(anyhow::Error::from)
    }

    pub fn shutdown_begin(self, remove_vtl0_vf: bool) -> HclNetworkVFManagerShutdownInProgress {
        self.shared_state
            .worker_channel
            .send(HclNetworkVfManagerMessage::ShutdownBegin(remove_vtl0_vf));
        HclNetworkVFManagerShutdownInProgress {
            inner: self,
            complete: false,
        }
    }
}

pub struct HclNetworkVFManagerShutdownInProgress {
    inner: HclNetworkVFManager,
    complete: bool,
}

impl Drop for HclNetworkVFManagerShutdownInProgress {
    fn drop(&mut self) {
        assert!(self.complete);
    }
}

impl HclNetworkVFManagerShutdownInProgress {
    pub async fn complete(&mut self, keep_vf_alive: bool) {
        if let Err(err) = self
            .inner
            .shared_state
            .worker_channel
            .call(HclNetworkVfManagerMessage::ShutdownComplete, keep_vf_alive)
            .await
        {
            tracing::error!(
                err = &err as &dyn std::error::Error,
                "Failure shutting down VF Manager"
            );
        }
        self.complete = true;
    }

    pub async fn save(mut self) -> Option<ManaSavedState> {
        let result = self.inner.save().await;
        self.complete = true;
        result
    }
}

struct HclNetworkVFManagerInstance<F> {
    guest_state: HclNetworkVFManagerGuestState,
    shared_state: Arc<HclNetworkVFManagerSharedState>,
    recv_update: mesh::Receiver<HclNetworkVFUpdateNotification>,
    set_vport_ready_and_get_vf_state: F,
}

impl<F> HclNetworkVFManagerInstance<F> {
    pub fn new(
        guest_state: HclNetworkVFManagerGuestState,
        shared_state: Arc<HclNetworkVFManagerSharedState>,
        recv_update: mesh::Receiver<HclNetworkVFUpdateNotification>,
        set_vport_ready_and_get_vf_state: F,
    ) -> Self {
        Self {
            guest_state,
            shared_state,
            recv_update,
            set_vport_ready_and_get_vf_state,
        }
    }
}

#[async_trait]
impl<F, R> netvsp::VirtualFunction for HclNetworkVFManagerInstance<F>
where
    F: Fn(bool) -> R + Sync + Send + 'static,
    R: Future<Output = bool> + Send,
{
    async fn id(&self) -> Option<u32> {
        self.guest_state.vtl0_vfid().await
    }

    async fn guest_ready_for_device(&mut self) {
        let should_be_offered =
            (self.set_vport_ready_and_get_vf_state)(self.guest_state.is_offered_to_guest().await)
                .await;
        if self.guest_state.is_offered_to_guest().await == should_be_offered {
            return;
        }

        if should_be_offered && self.id().await.is_none() {
            return;
        };

        if should_be_offered {
            self.shared_state
                .worker_channel
                .send(HclNetworkVfManagerMessage::AddVtl0VF);
        } else {
            self.shared_state
                .worker_channel
                .send(HclNetworkVfManagerMessage::RemoveVtl0VF);
        }
    }

    async fn wait_for_state_change(&mut self) -> Rpc<(), ()> {
        match self.recv_update.next().await {
            Some(HclNetworkVFUpdateNotification::Update(rpc)) => rpc,
            None => pending().await,
        }
    }
}

mod save_restore {
    use guid::Guid;
    use parking_lot::Mutex;
    use std::sync::Arc;

    pub mod state {
        use guid::Guid;
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;

        #[derive(Clone, Debug, Protobuf, SavedStateRoot)]
        #[mesh(package = "underhill.emuplat.netvsp")]
        pub struct SavedState {
            #[mesh(1)]
            pub instance_id: Guid,
            // The MANA device does not offer a mechanism to query the current
            // state of the VTL0 data path (MAC filter), so remember it here
            // for use when creating the device again on restore.
            #[mesh(2)]
            pub direction_to_vtl0: Vec<Option<bool>>,
            #[mesh(3)]
            pub hidden_vtl0: Option<bool>,
        }
    }

    #[derive(Clone)]
    pub struct RuntimeSavedState {
        pub instance_id: Guid,
        pub direction_to_vtl0: Arc<Mutex<Vec<Option<bool>>>>,
        pub hidden_vtl0: Arc<Mutex<Option<bool>>>,
    }

    impl RuntimeSavedState {
        pub fn new(instance_id: Guid) -> Self {
            Self {
                instance_id,
                direction_to_vtl0: Arc::new(Mutex::new(Vec::new())),
                hidden_vtl0: Arc::new(Mutex::new(Some(false))),
            }
        }

        pub fn direction_to_vtl0(&self, index: u32) -> Option<bool> {
            let index = index as usize;
            let direction_to_vtl0 = self.direction_to_vtl0.lock();
            if index < direction_to_vtl0.len() {
                direction_to_vtl0[index]
            } else {
                None
            }
        }

        pub fn vport_callback(&self, index: u32) -> Box<dyn Fn(bool) + Send + Sync> {
            let index = index as usize;
            let mut direction_to_vtl0 = self.direction_to_vtl0.lock();
            if direction_to_vtl0.len() <= index {
                direction_to_vtl0.resize(index + 1, None);
            }
            let this = self.clone();
            Box::new(move |to_vtl0: bool| {
                let mut direction_to_vtl0 = this.direction_to_vtl0.lock();
                direction_to_vtl0[index] = Some(to_vtl0);
            })
        }
    }

    impl From<&RuntimeSavedState> for state::SavedState {
        fn from(state: &RuntimeSavedState) -> Self {
            let direction_to_vtl0 = state.direction_to_vtl0.lock().to_vec();
            let hidden_vtl0 = *state.hidden_vtl0.lock();
            Self {
                instance_id: state.instance_id,
                direction_to_vtl0,
                hidden_vtl0,
            }
        }
    }

    impl From<&state::SavedState> for RuntimeSavedState {
        fn from(state: &state::SavedState) -> Self {
            let direction_to_vtl0 = Arc::new(Mutex::new(state.direction_to_vtl0.clone()));
            let hidden_vtl0 = Arc::new(Mutex::new(state.hidden_vtl0));
            Self {
                instance_id: state.instance_id,
                direction_to_vtl0,
                hidden_vtl0,
            }
        }
    }
}
