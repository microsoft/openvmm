// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::dispatch::vtl2_settings_worker::wait_for_pci_path;
use crate::options::KeepAliveConfig;
use crate::vpci::HclVpciBusControl;
use anyhow::Context;
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
pub use save_restore::RuntimeSavedState;
pub use save_restore::state::SavedState;
use socket2::Socket;
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
    vp_count: u32,
    max_sub_channels: u16,
    keepalive_mode: KeepAliveConfig,
    dma_clients: VfioDmaClients,
    mut mana_state: Option<&ManaSavedState>,
) -> anyhow::Result<ManaDevice<VfioDevice>> {
    // This guards from situations where we have saved state from keepalive
    // but the host does not support restoring it. In this case we log a warning,
    // free the memory, and continue with a fresh device.
    if mana_state.is_some() && !keepalive_mode.is_enabled() {
        tracing::warn!("have saved state from keepalive but restoring on an unsupported host");

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
    vp_count: u32,
    max_sub_channels: u16,
    dma_clients: VfioDmaClients,
    mana_state: Option<&ManaSavedState>,
) -> anyhow::Result<ManaDevice<VfioDevice>> {
    // Restore the device if we have saved state from servicing, otherwise create a new one.
    let device = if mana_state.is_some() {
        tracing::debug!("Restoring VFIO device from saved state");
        VfioDevice::restore(driver_source, pci_id, true, dma_clients)
            .instrument(tracing::info_span!("restore_mana_vfio_device"))
            .await
            .with_context(|| format!("failed to restore vfio device for {}", pci_id))?
    } else {
        VfioDevice::new(driver_source, pci_id, dma_clients)
            .instrument(tracing::info_span!("new_mana_vfio_device"))
            .await
            .with_context(|| format!("failed to open vfio device for {}", pci_id))?
    };

    ManaDevice::new(
        &driver_source.simple(),
        device,
        vp_count,
        max_sub_channels + 1,
        mana_state.map(|state| &state.mana_device),
    )
    .instrument(tracing::info_span!("new_mana_device"))
    .await
    .context("failed to initialize mana device")
}

fn vtl0_vfid_from_bus_control(vtl0_bus_control: &Vtl0Bus) -> Option<u32> {
    match vtl0_bus_control {
        Vtl0Bus::Present(bus_control) => Some(bus_control.instance_id().data1),
        _ => None,
    }
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
}

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
            },
            tx_to_worker,
        )
    }

    pub async fn connect_endpoints(&mut self) -> anyhow::Result<Vec<MacAddress>> {
        let device = self.mana_device.as_ref().expect("valid endpoint");
        let indices = (0..device.num_vports()).collect::<Vec<u32>>();
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
                            .context("failed to create mana vport")?;
                        let mac_address = vport.mac_address();
                        vport.set_serial_no(*index).await.with_context(|| {
                            format!("failed to set vport serial number {mac_address}")
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
                                format!("failed to connect new endpoint {mac_address}")
                            })?;
                        tracing::info!(%mac_address, "Network endpoint connected",);
                        anyhow::Ok((mac_address, control))
                    }
                },
            ),
        )
        .await?;
        let (addresses, pkt_capture_controls): (Vec<_>, Vec<_>) = result.into_iter().unzip();
        self.pkt_capture_controls = Some(pkt_capture_controls);
        Ok(addresses)
    }

    async fn send_vf_state_change_notifications(&self) -> anyhow::Result<()> {
        const MAX_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
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

    async fn try_notify_guest_and_revoke_vtl0_vf(&mut self, bus_control: &Vtl0Bus) {
        if !self.guest_state.is_offered_to_guest().await {
            return;
        }

        // Make removal request a no-op by setting offered to false. The actual removal will be done at the end of this
        // method.
        *self.guest_state.offered_to_guest.lock().await = false;
        // Give the network stack a chance to prepare for the removal.
        if let Err(err) = self.send_vf_state_change_notifications().await {
            tracing::error!(
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
                .await
                .into_iter()
                .collect::<anyhow::Result<Vec<_>, _>>()
            {
                tracing::error!(
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
            let bus_control = if let Vtl0Bus::Present(bus_control) = &bus_control {
                bus_control
            } else {
                let Vtl0Bus::Present(bus_control) = &self.vtl0_bus_control else {
                    unreachable!();
                };
                bus_control
            };
            bus_control.revoke_device().await
        } {
            tracing::error!(
                err = err.as_ref() as &dyn std::error::Error,
                "Failed to revoke VTL0 VF."
            );
        }
    }

    fn notify_vtl0_vf_arrival(&mut self) {
        // Notify the network stack of an arrival, but don't wait for a response.
        for update in self.guest_state_notifications.iter() {
            drop(update.call(HclNetworkVFUpdateNotification::Update, ()));
        }
    }

    pub async fn shutdown_vtl2_device(&mut self, keep_vf_alive: bool) {
        self.disconnect_all_endpoints().await;
        if let Some(device) = self.mana_device.take() {
            let (result, device) = device.shutdown().await;
            // Closing the VFIO device handle can take a long time. Leak the handle by
            // stashing it away.
            if keep_vf_alive {
                std::mem::forget(device);
            } else {
                if let Err(err) = result {
                    tracing::warn!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "Destroying MANA device"
                    );
                    // Enable FLR to try to recover the device.
                    match vfio_set_device_reset_method(&self.vtl2_pci_id, PciDeviceResetMethod::Flr)
                    {
                        Ok(_) => {
                            tracing::info!("Attempt to reset device via FLR on next teardown.");
                        }
                        Err(err) => {
                            tracing::warn!(
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

    async fn remove_vtl0_vf(&mut self) {
        if self.guest_state.is_offered_to_guest().await {
            *self.guest_state.offered_to_guest.lock().await = false;
            tracing::info!(
                vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                "Removing VF from VTL0"
            );
            if let Vtl0Bus::Present(vtl0_bus_control) = &self.vtl0_bus_control {
                match vtl0_bus_control.revoke_device().await {
                    Ok(_) => (),
                    Err(err) => {
                        tracing::error!(
                            err = err.as_ref() as &dyn std::error::Error,
                            "Failed to remove VTL0 VF"
                        );
                    }
                }
            }
        }
    }

    async fn disconnect_all_endpoints(&mut self) {
        futures::future::join_all(self.endpoint_controls.iter_mut().map(async |control| {
            match control.disconnect().await {
                Ok(Some(mut endpoint)) => {
                    tracing::info!("Network endpoint disconnected");
                    endpoint.stop().await;
                }
                Ok(None) => (),
                Err(err) => {
                    tracing::error!(
                        err = err.as_ref() as &dyn std::error::Error,
                        "Failed to disconnect endpoint"
                    );
                }
            }
        }))
        .await;
    }

    pub async fn run(&mut self) {
        #[derive(Debug)]
        enum NextWorkItem {
            Continue,
            ManagerMessage(HclNetworkVfManagerMessage),
            ManaDeviceArrived,
            ManaDeviceRemoved,
            ExitWorker,
        }

        let mut vtl2_device_present = true;
        loop {
            let next_work_item = {
                let next_message = self
                    .messages
                    .as_mut()
                    .unwrap()
                    .map(NextWorkItem::ManagerMessage)
                    .chain(iter([NextWorkItem::ExitWorker]));
                let device_change = self.vtl2_bus_control.notifier().map(|device| match device {
                    VpciBusEvent::DeviceEnumerated => {
                        tracing::info!("MANA device enumerated, waiting for uevent.");
                        NextWorkItem::Continue
                    }
                    VpciBusEvent::PrepareForRemoval => NextWorkItem::ManaDeviceRemoved,
                });
                let device_arrival = (&mut self.uevent_handler).map(|device_path| {
                    if !vtl2_device_present && Path::new(&device_path).exists() {
                        NextWorkItem::ManaDeviceArrived
                    } else {
                        NextWorkItem::Continue
                    }
                });
                (next_message, device_change, device_arrival)
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
                    .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::PacketCapture(rpc)) => {
                    rpc.handle_failable(async |params| self.handle_packet_capture(params).await)
                        .await
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::AddVtl0VF) => {
                    if self.is_shutdown_active {
                        continue;
                    }
                    if !self.guest_state.is_offered_to_guest().await
                        && self.guest_state.vtl0_vfid().await.is_some()
                    {
                        tracing::info!(
                            vfid = vtl0_vfid_from_bus_control(&self.vtl0_bus_control),
                            "Adding VF to VTL0"
                        );
                        if let Vtl0Bus::Present(vtl0_bus_control) = &self.vtl0_bus_control {
                            match vtl0_bus_control.offer_device().await {
                                Ok(_) => {
                                    *self.guest_state.offered_to_guest.lock().await = true;
                                }
                                Err(err) => {
                                    tracing::error!(
                                        err = err.as_ref() as &dyn std::error::Error,
                                        "Failed to add VTL0 VF"
                                    );
                                }
                            }
                        }
                    }
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::RemoveVtl0VF) => {
                    if self.is_shutdown_active {
                        continue;
                    }
                    self.remove_vtl0_vf().await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::UpdateVtl0VF(rpc)) => {
                    if self.is_shutdown_active {
                        rpc.complete(());
                        continue;
                    }
                    rpc.handle(async |bus_control| {
                        let is_present = matches!(
                            self.vtl0_bus_control,
                            Vtl0Bus::Present(_) | Vtl0Bus::HiddenPresent(_)
                        );
                        assert!(is_present != bus_control.is_some());
                        tracing::info!(present = bus_control.is_some(), "VTL0 VF device change");
                        if matches!(&self.vtl0_bus_control, Vtl0Bus::HiddenNotPresent) {
                            self.vtl0_bus_control = Vtl0Bus::HiddenPresent(bus_control.unwrap())
                        } else if matches!(&self.vtl0_bus_control, Vtl0Bus::HiddenPresent(_)) {
                            self.vtl0_bus_control = Vtl0Bus::HiddenNotPresent;
                        } else if vtl2_device_present {
                            let bus_control = bus_control
                                .map(Vtl0Bus::Present)
                                .unwrap_or(Vtl0Bus::NotPresent);
                            *self.guest_state.vtl0_vfid.lock().await =
                                vtl0_vfid_from_bus_control(&bus_control);
                            let old_bus_control =
                                std::mem::replace(&mut self.vtl0_bus_control, bus_control);
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
                    .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::HideVtl0VF(rpc)) => {
                    if self.is_shutdown_active {
                        rpc.complete(());
                        continue;
                    }
                    rpc.handle(async |hide_vtl0| {
                        tracing::info!(hide_vtl0, "VTL0 VF device is hidden");
                        if hide_vtl0 {
                            *self.save_state.hidden_vtl0.lock() = Some(true);
                            if !matches!(self.vtl0_bus_control, Vtl0Bus::HiddenPresent(_)) {
                                let old_bus_control = std::mem::replace(
                                    &mut self.vtl0_bus_control,
                                    Vtl0Bus::HiddenNotPresent,
                                );
                                if matches!(old_bus_control, Vtl0Bus::Present(_)) {
                                    if vtl2_device_present {
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
                            *self.save_state.hidden_vtl0.lock() = Some(false);
                            if matches!(self.vtl0_bus_control, Vtl0Bus::HiddenPresent(_)) {
                                let Vtl0Bus::HiddenPresent(bus_control) = std::mem::replace(
                                    &mut self.vtl0_bus_control,
                                    Vtl0Bus::NotPresent,
                                ) else {
                                    unreachable!();
                                };
                                self.vtl0_bus_control = Vtl0Bus::Present(bus_control);
                                if vtl2_device_present {
                                    *self.guest_state.vtl0_vfid.lock().await =
                                        vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                                    self.notify_vtl0_vf_arrival();
                                }
                            } else if matches!(self.vtl0_bus_control, Vtl0Bus::HiddenNotPresent) {
                                self.vtl0_bus_control = Vtl0Bus::NotPresent;
                            }
                        }
                    })
                    .await;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::SaveState(rpc)) => {
                    assert!(self.is_shutdown_active);
                    drop(self.messages.take().unwrap());
                    rpc.handle(async |_| {
                        self.disconnect_all_endpoints().await;

                        if let Some(device) = self.mana_device.take() {
                            let (saved_state, device) = device.save().await;

                            // Closing the VFIO device handle can take a long time.
                            // Leak the handle by stashing it away.
                            std::mem::forget(device);

                            match saved_state {
                                Ok(saved_state) => VfManagerSaveResult::Saved(ManaSavedState {
                                    mana_device: saved_state,
                                    pci_id: self.vtl2_pci_id.clone(),
                                }),
                                Err(_) => {
                                    tracing::error!("Failed while saving MANA device state");
                                    VfManagerSaveResult::SaveFailed
                                }
                            }
                        } else {
                            tracing::warn!("no MANA device present when saving state");
                            VfManagerSaveResult::DeviceMissing
                        }
                    })
                    .await;
                    // Exit worker thread.
                    return;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::ShutdownBegin(
                    remove_vtl0_vf,
                )) => {
                    if remove_vtl0_vf {
                        self.remove_vtl0_vf().await;
                    }
                    self.is_shutdown_active = true;
                }
                NextWorkItem::ManagerMessage(HclNetworkVfManagerMessage::ShutdownComplete(rpc)) => {
                    tracing::info!("shutting down VTL2 device");
                    assert!(self.is_shutdown_active);
                    drop(self.messages.take().unwrap());
                    rpc.handle(async |keep_vf_alive| {
                        self.shutdown_vtl2_device(keep_vf_alive).await;
                    })
                    .await;
                    // Exit worker thread.
                    return;
                }
                NextWorkItem::ManaDeviceArrived => {
                    assert!(!self.is_shutdown_active);
                    let mut ctx =
                        mesh::CancelContext::new().with_timeout(std::time::Duration::from_secs(1));
                    // Ignore error here for waiting for the PCI path and continue to create the MANA device.
                    if ctx
                        .until_cancelled(wait_for_pci_path(&self.vtl2_pci_id))
                        .await
                        .is_err()
                    {
                        let pci_path = Path::new("/sys/bus/pci/devices").join(&self.vtl2_pci_id);
                        tracing::error!(?pci_path, "Timed out waiting for MANA PCI path");
                    } else {
                        tracing::info!("VTL2 VF arrived");
                    }

                    let device_bound = match create_mana_device(
                        &self.driver_source,
                        &self.vtl2_pci_id,
                        self.vp_count,
                        self.max_sub_channels,
                        KeepAliveConfig::Disabled,
                        self.dma_clients.clone(),
                        None, // No saved state on new device arrival
                    )
                    .await
                    {
                        Ok(device) => {
                            self.mana_device = Some(device);
                            self.connect_endpoints().await.is_ok()
                        }
                        Err(err) => {
                            tracing::error!(
                                err = err.as_ref() as &dyn std::error::Error,
                                "Failed to create MANA device"
                            );
                            false
                        }
                    };
                    if let Err(err) = self
                        .vtl2_bus_control
                        .update_vtl2_device_bind_state(device_bound)
                        .await
                    {
                        tracing::error!(
                            err = err.as_ref() as &dyn std::error::Error,
                            "Failed to report new binding state to host"
                        );
                    }
                    if device_bound {
                        vtl2_device_present = true;
                        if matches!(&self.vtl0_bus_control, Vtl0Bus::Present(_)) {
                            *self.guest_state.vtl0_vfid.lock().await =
                                vtl0_vfid_from_bus_control(&self.vtl0_bus_control);
                            self.notify_vtl0_vf_arrival();
                        }
                    }
                }
                NextWorkItem::ManaDeviceRemoved => {
                    assert!(!self.is_shutdown_active);
                    tracing::info!("VTL2 VF being removed");
                    *self.guest_state.vtl0_vfid.lock().await = None;
                    if self.guest_state.is_offered_to_guest().await {
                        tracing::warn!("VTL0 VF being removed as a result of VTL2 VF revoke.");
                        self.try_notify_guest_and_revoke_vtl0_vf(&Vtl0Bus::NotPresent)
                            .await;
                    }

                    self.shutdown_vtl2_device(false).await;
                    vtl2_device_present = false;

                    if let Err(err) = self
                        .vtl2_bus_control
                        .update_vtl2_device_bind_state(false)
                        .await
                    {
                        tracing::error!(
                            err = err.as_ref() as &dyn std::error::Error,
                            "Failed to report new binding state to host"
                        );
                    }
                }
                NextWorkItem::ExitWorker => {
                    drop(self.messages.take().unwrap());
                    tracing::info!(pci_id = &self.vtl2_pci_id, "Worker exiting");
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

struct HclNetworkVfManagerUeventHandler {
    uevent_receiver: mesh::Receiver<String>,
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
            // uevent can also notify rescan events, in which case we don't know here whether
            // it is an add or remove. Just wake up the receiver and let the receiver decide
            // how to handle that case.
            let action = uevent.get("ACTION").unwrap_or("unknown");
            let dev_path = uevent.get("DEVPATH").unwrap_or("unknown");
            if device_path == dev_path {
                if action == "add" || action == "remove" {
                    tx.send(fs_dev_path.clone());
                }
            } else if uevent.get("RESCAN") == Some("true") {
                tx.send(fs_dev_path.clone());
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
    type Item = String;

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
    ) -> anyhow::Result<(
        Self,
        Vec<HclNetworkVFManagerEndpointInfo>,
        RuntimeSavedState,
    )> {
        let device = create_mana_device(
            driver_source,
            &vtl2_pci_id,
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
        );

        // Queue new endpoints.
        let mac_addresses = worker.connect_endpoints().await?;
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
        worker
            .mana_device
            .as_mut()
            .unwrap()
            .start_notification_task(driver_source)
            .await;
        let endpoints = endpoints
            .into_iter()
            .zip(mac_addresses)
            .enumerate()
            .map(
                |(i, (endpoint, mac_address))| HclNetworkVFManagerEndpointInfo {
                    adapter_index: i as u32,
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
            let direction_to_vtl0 = Arc::new(Mutex::new(state.direction_to_vtl0.to_vec()));
            let hidden_vtl0 = Arc::new(Mutex::new(state.hidden_vtl0));
            Self {
                instance_id: state.instance_id,
                direction_to_vtl0,
                hidden_vtl0,
            }
        }
    }
}
