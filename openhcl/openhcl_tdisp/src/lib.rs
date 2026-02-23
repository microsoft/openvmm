// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides resources and traits for a TDISP client device
//! interface for OpenHCL devices.
//!
//! See: `vm/devices/tdisp` for more information.

use std::future::Future;

// Re-export the TDISP protocol types necessary for OpenbHCL from top level tdisp crates
// to avoid a direct dependency on tdisp_proto and tdisp.
pub use tdisp::TdispGuestOperationError;
pub use tdisp::serialize_proto::deserialize_command;
pub use tdisp::serialize_proto::deserialize_response;
pub use tdisp::serialize_proto::serialize_command;
pub use tdisp::serialize_proto::serialize_response;
pub use tdisp_proto::GuestToHostCommand;
pub use tdisp_proto::GuestToHostCommandExt;
pub use tdisp_proto::GuestToHostResponse;
pub use tdisp_proto::GuestToHostResponseExt;
pub use tdisp_proto::TdiReportStruct;
pub use tdisp_proto::TdispCommandRequestGetDeviceInterfaceInfo;
pub use tdisp_proto::TdispCommandResponseGetDeviceInterfaceInfo;
pub use tdisp_proto::TdispDeviceInterfaceInfo;
pub use tdisp_proto::TdispGuestOperationErrorCode;
pub use tdisp_proto::TdispGuestProtocolType;
pub use tdisp_proto::TdispGuestUnbindReason;
pub use tdisp_proto::TdispReportType;

use tdisp_proto::guest_to_host_command::Command;

/// Represents a TDISP device assigned to a guest partition. This trait allows
/// implementations to send TDISP commands to the host through a backing interface
/// such as a VPCI channel.
///
pub trait TdispVirtualDeviceInterface: Send + Sync {
    /// Sends a TDISP command to the device through the VPCI channel.
    fn send_tdisp_command(
        &self,
        payload: GuestToHostCommand,
    ) -> impl Future<Output = Result<GuestToHostResponse, anyhow::Error>> + Send;

    /// Get the TDISP interface info for the device.
    fn tdisp_get_device_interface_info(
        &self,
    ) -> impl Future<Output = anyhow::Result<TdispDeviceInterfaceInfo>> + Send;

    /// Bind the device to the current partition and transition to Locked.
    /// NOTE: While the device is in the Locked state, it can continue to
    /// perform unencrypted operations until it is moved to the Running state.
    /// The Locked state is a transitional state that is designed to keep
    /// the device from modifying its resources prior to attestation.
    fn tdisp_bind_interface(&self) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Start a bound device by transitioning it to the Run state from the Locked state.
    /// This allows for attestation and for resources to be accepted into the guest context.
    fn tdisp_start_device(&self) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Request a device report from the TDI or physical device depending on the report type.
    fn tdisp_get_device_report(
        &self,
        report_type: &TdispReportType,
    ) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send;

    /// Request a TDI report from the TDI or physical device.
    fn tdisp_get_tdi_report(&self) -> impl Future<Output = anyhow::Result<TdiReportStruct>> + Send;

    /// Request the TDI device id from the vpci channel.
    fn tdisp_get_tdi_device_id(&self) -> impl Future<Output = anyhow::Result<u64>> + Send;

    /// Request to unbind the device and return to the Unlocked state.
    fn tdisp_unbind(
        &self,
        reason: TdispGuestUnbindReason,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Creates a [`GuestToHostCommand`] for the `GetDeviceInterfaceInfo` command.
pub fn make_get_device_interface_info_command(
    device_id: u64,
    guest_protocol_type: TdispGuestProtocolType,
) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::GetDeviceInterfaceInfo(
            TdispCommandRequestGetDeviceInterfaceInfo {
                guest_protocol_type: guest_protocol_type as i32,
            },
        )),
    }
}
