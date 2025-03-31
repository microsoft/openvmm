// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

//! Implements shutdown relay device between guest and host.

use futures::FutureExt;
use futures::StreamExt;
use hyperv_ic_resources::shutdown::ShutdownParams;
use hyperv_ic_resources::shutdown::ShutdownResult;
use hyperv_ic_resources::shutdown::ShutdownRpc;
use mesh::rpc::PendingRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use state_unit::SpawnedUnit;
use state_unit::StateUnits;
use vmbus_channel::channel::VmbusDevice;
use vmbus_relay_intercept_device::InterceptDeviceVmbusControl;
use vmcore::vm_task::VmTaskDriverSource;
use vmm_core::vmbus_unit::ChannelUnit;
use vmm_core::vmbus_unit::VmbusServerHandle;
use vmm_core::vmbus_unit::offer_generic_channel_unit;

/// The current vmbus state of the guest portion of the shutdown relay device.
enum ShutdownRelayDeviceVmbusState {
    Offered(SpawnedUnit<ChannelUnit<dyn VmbusDevice>>),
    Revoked(Box<dyn VmbusDevice>),
    None,
}

/// The current state of the guest/VTL0 vmbus device.
enum ShutdownConnectionState {
    Unregistered,
    Disconnected(
        PendingRpc<(
            mesh_channel::MpscSender<Rpc<ShutdownParams, ShutdownResult>>,
            mesh::OneshotReceiver<()>,
        )>,
    ),
    Connected(
        (
            mesh_channel::MpscSender<Rpc<ShutdownParams, ShutdownResult>>,
            mesh::OneshotReceiver<()>,
        ),
    ),
}

/// Tracks state of the shutdown relay connecting the host visible shutdown
/// device and the guest visible shutdown device.
pub(crate) struct ShutdownRelayDevice {
    pub driver: VmTaskDriverSource,
    pub host_client_control: InterceptDeviceVmbusControl,
    pub host_notification: mesh::Receiver<Rpc<ShutdownParams, ShutdownResult>>,
    pub guest_notifier: mesh::Sender<ShutdownRpc>,
    vmbus_state: ShutdownRelayDeviceVmbusState,
    shutdown_connection_state: ShutdownConnectionState,
}

/// Event messages from the shutdown relay.
pub(crate) enum ShutdownRelayMessage {
    GuestConnectivityChange(Result<bool, mesh::RecvError>),
    ShutdownRequest(Rpc<ShutdownParams, ShutdownResult>),
}

impl ShutdownRelayDevice {
    /// Create a new instance.
    pub fn new(
        driver: VmTaskDriverSource,
        host_client_control: InterceptDeviceVmbusControl,
        host_notification: mesh::Receiver<Rpc<ShutdownParams, ShutdownResult>>,
        guest_notifier: mesh::Sender<ShutdownRpc>,
        shutdown_device: SpawnedUnit<ChannelUnit<dyn VmbusDevice>>,
    ) -> Self {
        Self {
            driver,
            host_client_control,
            host_notification,
            guest_notifier,
            vmbus_state: ShutdownRelayDeviceVmbusState::Offered(shutdown_device),
            shutdown_connection_state: ShutdownConnectionState::Unregistered,
        }
    }

    /// Prepare for VM start.
    pub async fn start(&mut self, state_units: &StateUnits, vmbus: &VmbusServerHandle) {
        if matches!(self.vmbus_state, ShutdownRelayDeviceVmbusState::Revoked(_)) {
            let revoked =
                std::mem::replace(&mut self.vmbus_state, ShutdownRelayDeviceVmbusState::None);
            let ShutdownRelayDeviceVmbusState::Revoked(vmbus_device) = revoked else {
                panic!("shutdown relay not in revoked state");
            };
            match offer_generic_channel_unit(&self.driver, state_units, vmbus, vmbus_device).await {
                Ok(device_unit) => {
                    let _ = std::mem::replace(
                        &mut self.vmbus_state,
                        ShutdownRelayDeviceVmbusState::Offered(device_unit),
                    );
                }
                Err(err) => {
                    tracing::error!(
                        error = err.as_ref() as &dyn std::error::Error,
                        "Failed to start shutdown relay device"
                    );
                }
            };
        }
    }

    /// Prepare for VM stop.
    pub async fn stop(&mut self) {
        // The version of openhcl that the VTL0 guest resumes with may not
        // support the shutdown relay device, so always remove it during
        // stop. It will be recreated on the next start if supported.
        if matches!(self.vmbus_state, ShutdownRelayDeviceVmbusState::Offered(_)) {
            let offered =
                std::mem::replace(&mut self.vmbus_state, ShutdownRelayDeviceVmbusState::None);
            let ShutdownRelayDeviceVmbusState::Offered(vmbus_device) = offered else {
                panic!("shutdown relay not in offered state");
            };
            let shutdown_ic = vmbus_device.remove().await.revoke_generic().await;
            let _ = std::mem::replace(
                &mut self.vmbus_state,
                ShutdownRelayDeviceVmbusState::Revoked(shutdown_ic),
            );
        }
    }

    /// Fetch the next shutdown relay event.
    pub async fn next_message(&mut self) -> ShutdownRelayMessage {
        futures::select! {
            message = async {
                if matches!(self.shutdown_connection_state, ShutdownConnectionState::Unregistered) {
                    self.shutdown_connection_state = ShutdownConnectionState::Disconnected(self.guest_notifier.call(ShutdownRpc::WaitReady, ()));
                }
                let result = match &mut self.shutdown_connection_state {
                    ShutdownConnectionState::Disconnected(rpc) => {
                        match rpc.await {
                            Ok(result) => Ok(ShutdownConnectionState::Connected(result)),
                            Err(RpcError::Channel(err)) => Err(err),
                            _ => Err(mesh::RecvError::Closed),
                        }
                    }
                    ShutdownConnectionState::Connected((_, rpc)) => {
                        match rpc.await {
                            Ok(()) | Err(mesh::RecvError::Closed) => Ok(ShutdownConnectionState::Disconnected(self.guest_notifier.call(ShutdownRpc::WaitReady, ()))),
                            Err(err) => Err(err),
                        }
                    }
                    ShutdownConnectionState::Unregistered => unreachable!(),
                };
                if result.is_ok() {
                    self.shutdown_connection_state = result.unwrap();
                    Ok(matches!(self.shutdown_connection_state, ShutdownConnectionState::Connected(_)))
                } else {
                    result.map(|_| false)
                }
            }.fuse() => ShutdownRelayMessage::GuestConnectivityChange(message),
            message = self.host_notification.select_next_some() => ShutdownRelayMessage::ShutdownRequest(message)
        }
    }

    /// Connect the host visible shutdown device.
    pub fn connect_to_host(&mut self) {
        self.host_client_control.connect();
    }

    /// Disconnect the host visible shutdown device.
    pub fn disconnect_from_host(&mut self) {
        self.host_client_control.disconnect();
    }

    /// Send a shutdown message to the guest-visible shutdown device and return
    /// the result.
    pub async fn send_shutdown_to_guest(&self, params: ShutdownParams) -> ShutdownResult {
        let ShutdownConnectionState::Connected((notifier, _)) = &self.shutdown_connection_state
        else {
            tracing::info!("Shutdown message cannot be relayed when device is not connected");
            return ShutdownResult::NotReady;
        };
        tracing::info!(?params, "Relaying shutdown message");
        let result = match notifier.call(|x| x, params).await {
            Ok(result) => result,
            Err(RpcError::Channel(mesh::RecvError::Closed)) => ShutdownResult::NotReady,
            Err(err) => {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "Failed to relay shutdown notification to guest"
                );
                ShutdownResult::Failed(hyperv_ic_guest::shutdown::E_FAIL)
            }
        };
        match result {
            ShutdownResult::Ok => (),
            ShutdownResult::AlreadyInProgress => {
                tracing::warn!("shutdown request already in progress");
            }
            ShutdownResult::Failed(_) => {
                tracing::warn!(?result, "Shutdown request failed");
            }
            ShutdownResult::NotReady => {
                tracing::warn!("Guest shutdown channel not connected");
            }
        };
        result
    }
}
