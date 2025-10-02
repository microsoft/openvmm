// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//!
//! This module provides resources and traits for a TDISP client device
//! interface for assigned devices in OpenHCL.
//!
//! See: `vm/tdisp` for more information.

pub use tdisp::TdispCommandId;
pub use tdisp::{TDISP_INTERFACE_VERSION_MAJOR, TDISP_INTERFACE_VERSION_MINOR};

use inspect::Inspect;
use std::future::Future;
use tdisp::GuestToHostCommand;
use tdisp::GuestToHostResponse;
use tdisp::TdispGuestUnbindReason;
use tdisp::devicereport::TdiReportStruct;
use tdisp::devicereport::TdispDeviceReportType;

/// Represents a TDISP device assigned to a guest partition. This trait allows
/// the guest to send TDISP commands to the host through the backing interface.
/// [TDISP TODO] Change out `anyhow` for a `TdispError` type?
pub trait ClientDevice: Send + Sync + Inspect {
    /// Send a TDISP command to the host through the backing interface.
    fn tdisp_command_to_host(
        &self,
        command: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse>;

    /// Checks if the device is TDISP capable and returns the device interface info if so.
    fn tdisp_get_device_interface_info(&self) -> anyhow::Result<tdisp::TdispDeviceInterfaceInfo>;

    /// Bind the device to the current partition and transition to Locked.
    fn tdisp_bind_interface(&self) -> anyhow::Result<()>;
}

/// Represents a TDISP device assigned to a guest partition that can be used to
/// send TDISP commands to the host through a backing interface.
pub trait VpciTdispInterface: Send + Sync {
    /// Sends a TDISP command to the device through the VPCI channel.
    fn send_tdisp_command(
        &self,
        payload: GuestToHostCommand,
    ) -> impl Future<Output = Result<GuestToHostResponse, anyhow::Error>> + Send;

    /// Get the TDISP interface info for the device.
    fn tdisp_get_device_interface_info(
        &self,
    ) -> impl Future<Output = anyhow::Result<tdisp::TdispDeviceInterfaceInfo>> + Send;

    /// Bind the device to the current partition and transition to Locked.
    /// NOTE: While the device is in the Locked state, it can continue to
    /// perform unencrypted operations until it is moved to the Running state.
    /// The Locked state is a transitional state that is designed to keep
    /// the device from modifying its resources prior to attestation.
    fn tdisp_bind_interface(&self) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Start a bound device by transitioning it to the from the Locked state to the Run state.
    /// This allows for attestation and for resources to be accepted into the guest context.
    fn tdisp_start_device(&self) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Request a device report from the TDI or physical device depending on the report type.
    fn tdisp_get_device_report(
        &self,
        report_type: &TdispDeviceReportType,
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
