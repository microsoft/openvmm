// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! This module provides resources and traits for a TDISP client device
//! interface for OpenHCL devices.
//!
//! See: `vm/devices/tdisp` for more information.

#[cfg(feature = "dev_snp_ohcl_tio_support")]
mod sevtio;

pub mod mocks;

// Re-export the TDISP protocol types necessary for OpenHCL from top level tdisp crates
// to avoid a direct dependency on tdisp_proto and tdisp.
pub use tdisp::TdispGuestOperationError;
pub use tdisp::devicereport::TdiReportStruct;
pub use tdisp::serialize_proto::deserialize_command;
pub use tdisp::serialize_proto::deserialize_response;
pub use tdisp::serialize_proto::serialize_command;
pub use tdisp::serialize_proto::serialize_response;
pub use tdisp_proto::GuestToHostCommand;
pub use tdisp_proto::GuestToHostCommandExt;
pub use tdisp_proto::GuestToHostResponse;
pub use tdisp_proto::GuestToHostResponseExt;
pub use tdisp_proto::TdispCommandRequestGetDeviceInterfaceInfo;
pub use tdisp_proto::TdispCommandResponseBind;
pub use tdisp_proto::TdispCommandResponseGetDeviceInterfaceInfo;
pub use tdisp_proto::TdispCommandResponseGetTdiReport;
pub use tdisp_proto::TdispCommandResponseStartTdi;
pub use tdisp_proto::TdispCommandResponseUnbind;
pub use tdisp_proto::TdispDeviceInterfaceInfo;
pub use tdisp_proto::TdispGuestOperationErrorCode;
pub use tdisp_proto::TdispGuestProtocolType;
pub use tdisp_proto::TdispGuestUnbindReason;
pub use tdisp_proto::TdispReportType;
pub use tdisp_proto::TdispTdiState;

#[cfg(feature = "dev_snp_ohcl_tio_support")]
pub use sevtio::TdispSevTioResourceValidator;

use hvdef::Vtl;
use tdisp_proto::TdispCommandRequestBind;
use tdisp_proto::TdispCommandRequestGetTdiReport;
use tdisp_proto::TdispCommandRequestStartTdi;
use tdisp_proto::TdispCommandRequestUnbind;
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
        target_protocol: TdispGuestProtocolType,
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

/// Provides platform-specific methods for unblocking device resources after
/// TDISP attestation.
///
/// After a device has been attested and placed in the Run state via
/// [`TdispVirtualDeviceInterface`], platform-specific operations are required
/// to make device resources (MMIO, DMA) accessible to the guest. This trait
/// abstracts those operations.
pub trait TdispResourceValidationInterface: Send + Sync {
    /// Unblock MMIO access for a specific resource on the device.
    ///
    /// * `device_id` - Identifies the TDI device (not a VPCI ID).
    /// * `range_id` - Identifies which MMIO range to unblock.
    /// * `base_gpa` - The base guest physical address of the MMIO range to unblock.
    /// * `base_offset` - The offset within the range specified by `range_id` to start
    ///   unblocking from. Necessary for cases where the host splits the MMIO range
    ///   into multiple subranges for unblocking.
    /// * `length_in_bytes` - The length in bytes of the MMIO range to unblock starting from `base_offset`.
    fn tdisp_unblock_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> anyhow::Result<()>;

    /// Unblock DMA access for the device's IOMMU domain.
    ///
    /// * `device_id` - Identifies the TDI device (not a VPCI ID).
    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()>;

    /// Re-block a previously-unblocked MMIO range. This is the inverse
    /// of [`Self::tdisp_unblock_mmio`] and is called during unbind so
    /// the guest-private pages are flipped back to shared (host-visible)
    /// before the device channel is torn down.
    ///
    /// Arguments mirror [`Self::tdisp_unblock_mmio`].
    fn tdisp_block_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> anyhow::Result<()>;

    /// Re-block DMA access. Inverse of [`Self::tdisp_unblock_dma`].
    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()>;

    /// Query the platform firmware for the current TDI state of the device.
    ///
    /// Returns `Ok(None)` on platforms that do not support querying firmware
    /// TDI state directly. On platforms that do (e.g. SEV-TIO), returns
    /// `Ok(Some(state))` with the firmware's state mapped to
    /// [`TdispTdiState`], or `Err` if the firmware reports an error state
    /// or the request itself fails.
    ///
    /// Callers can use this to validate that the paravisor's cached TDI
    /// state matches what the firmware actually has before sending
    /// state-sensitive commands to the host.
    ///
    /// * `device_id` - Identifies the TDI device (not a VPCI ID).
    fn tdisp_query_firmware_tdi_state(
        &self,
        device_id: u16,
    ) -> anyhow::Result<Option<TdispTdiState>> {
        let _ = device_id;
        Ok(None)
    }
}

/// Creates a [`GuestToHostCommand`] for the `GetDeviceInterfaceInfo` command.
pub fn new_get_device_interface_info_command(
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

/// Creates a [`GuestToHostCommand`] for the `Bind` command.
pub fn new_bind_command(device_id: u64) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::Bind(TdispCommandRequestBind {})),
    }
}

/// Creates a [`GuestToHostCommand`] for the `StartTdi` command.
pub fn new_start_tdi_command(device_id: u64) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::StartTdi(TdispCommandRequestStartTdi {})),
    }
}

/// Creates a [`GuestToHostCommand`] for the `GetTdiReport` command.
pub fn new_get_tdi_report_command(
    device_id: u64,
    report_type: TdispReportType,
) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::GetTdiReport(TdispCommandRequestGetTdiReport {
            report_type: report_type as i32,
        })),
    }
}

/// Creates a [`GuestToHostCommand`] for the `Unbind` command.
pub fn new_unbind_command(device_id: u64, reason: TdispGuestUnbindReason) -> GuestToHostCommand {
    GuestToHostCommand {
        device_id,
        command: Some(Command::Unbind(TdispCommandRequestUnbind {
            unbind_reason: reason as i32,
        })),
    }
}
