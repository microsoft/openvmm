// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDISP interface implementation for VPCI devices.

use anyhow::Context;
use hvdef::Vtl;
use inspect::Inspect;
use mesh::rpc::RpcSend;
use openhcl_tdisp::GuestToHostCommand;
use openhcl_tdisp::GuestToHostCommandExt;
use openhcl_tdisp::GuestToHostResponse;
use openhcl_tdisp::GuestToHostResponseExt;
use openhcl_tdisp::TdispCommandResponseBind;
use openhcl_tdisp::TdispCommandResponseGetDeviceInterfaceInfo;
use openhcl_tdisp::TdispCommandResponseGetTdiReport;
use openhcl_tdisp::TdispCommandResponseModifyMmioRange;
use openhcl_tdisp::TdispCommandResponseStartTdi;
use openhcl_tdisp::TdispCommandResponseUnbind;
use openhcl_tdisp::TdispDeviceInterfaceInfo;
use openhcl_tdisp::TdispGuestOperationErrorCode;
use openhcl_tdisp::TdispGuestProtocolType;
use openhcl_tdisp::TdispGuestUnbindReason;
use openhcl_tdisp::TdispHostCommandSender;
use openhcl_tdisp::TdispReportType;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use std::future::Future;
use std::pin::Pin;
use tdisp::TdispIsolationReport;
use tdisp::TdispResourceIsolation;
use tdisp::TdispTdiState;
use tdisp::devicereport::TdiReportStruct;
use virt::IsolationType;
use vpci_protocol::MAX_VPCI_TDISP_COMMAND_SIZE;
use vpci_protocol::SlotNumber;

use super::VpciDevice;
use super::WorkerRequest;
use openhcl_tdisp::TdispResourceValidationInterface;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Inspect)]
struct VpciClientTdispMutableState {
    tdi_state: TdispTdiState,
    #[inspect(debug)]
    guest_device_id: TdispDeviceId,
    /// Map of BAR ID to the range the guest configured and how it was
    /// classified. Populated whenever a BAR is reconfigured in the `Run`
    /// state, both for ranges that were unblocked and for ones that were
    /// deliberately skipped. Used during unbind to call `tdisp_block_mmio`
    /// with the same parameters, for the ranges that need it, so private
    /// pages can be flipped back to shared. Cleared on unbind.
    #[inspect(iter_by_key)]
    validated_mmio_bars: std::collections::HashMap<u16, ValidatedMmio>,
    /// Whether DMA has been unblocked via `tdisp_unblock_dma`. Cleared on
    /// unbind so that DMA is re-unblocked after re-attestation.
    dma_unblocked: bool,
    /// The most recently obtained TDI interface report, populated during attestation.
    /// Cleared on unbind so that it is re-fetched after re-attestation.
    #[inspect(debug)]
    tdi_report: Option<TdiReportStruct>,
    /// Set of BAR IDs whose MMIO pages are intercepted (e.g. a BAR that hosts
    /// the MSI-X table / PBA emulated by the host). These pages are not backed
    /// by guest RAM on the host.
    #[inspect(iter_by_index)]
    intercepted_bars: HashSet<u16>,
}

/// Identifies the TDI device, as distinct from the VPCI slot id.
///
/// A TDI only has an id once attestation has fetched one from the host, and it
/// loses it again on unbind, so "no id" is a real state of the device rather
/// than a particular id value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TdispDeviceId {
    /// No TDI has been identified: attestation has not fetched an id yet, or
    /// unbind has dropped the one it had.
    Invalid,
    /// The TDI device id the host reported.
    Valid(u16),
}

impl TdispDeviceId {
    /// The underlying device id, or `None` when no TDI has been identified.
    /// Platform interfaces address a real TDI, so they take the inner value.
    fn id(self) -> Option<u16> {
        match self {
            TdispDeviceId::Invalid => None,
            TdispDeviceId::Valid(device_id) => Some(device_id),
        }
    }
}

/// Tracks how a BAR's MMIO range was handled when the guest reconfigured it,
/// so unbind knows whether the range needs blocking back and with what
/// parameters.
#[derive(Inspect, Clone, Copy, Debug)]
struct ValidatedMmio {
    #[inspect(hex)]
    base_gpa: u64,
    #[inspect(hex)]
    length_in_bytes: u64,
    /// How the range was classified. `Private` means it was passed to
    /// `tdisp_unblock_mmio` and must be blocked back on unbind; `Shared`
    /// means it was deliberately skipped and there is nothing to undo.
    #[inspect(debug)]
    isolation: TdispResourceIsolation,
}

/// Sends guest-to-host TDISP commands for one device, handed to the resource
/// validator so platform code can issue commands without owning the channel.
///
/// This does not track the TDI's state, so it suits only commands that do not
/// transition the TDI.
struct VpciTdispHostSender {
    worker_req: mesh::Sender<WorkerRequest>,
    vpci_device_id: u64,
}

impl TdispHostCommandSender for VpciTdispHostSender {
    fn send_tdisp_command<'a>(
        &'a self,
        mut command: GuestToHostCommand,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<GuestToHostResponse>> + Send + Sync + 'a>> {
        Box::pin(async move {
            // The validator does not know the VPCI slot, so address the command
            // here rather than requiring every caller to supply it.
            command.device_id = self.vpci_device_id;

            let serialized = openhcl_tdisp::serialize_command(&command);
            if serialized.len() > MAX_VPCI_TDISP_COMMAND_SIZE {
                anyhow::bail!(
                    "serialized TDISP command exceeds VMBUS maximum packet size ({} > {})",
                    serialized.len(),
                    MAX_VPCI_TDISP_COMMAND_SIZE
                );
            }

            self.worker_req
                .call_failable(
                    WorkerRequest::TdispCommand,
                    vpci_protocol::VpciTdispCommand {
                        header: vpci_protocol::VpciTdispCommandHeader {
                            message_type: vpci_protocol::MessageType::VPCI_TDISP_COMMAND,
                            slot: SlotNumber::from_bits(self.vpci_device_id as u32),
                            data_length: serialized.len() as u64,
                        },
                        data: serialized,
                    },
                )
                .await
                .map_err(|err: mesh::rpc::RpcError<mesh::error::RemoteError>| {
                    tracing::error!(
                        error = &err as &dyn std::error::Error,
                        "failed to send tdisp command"
                    );
                    anyhow::anyhow!("failed to send tdisp command")
                })
        })
    }
}

impl VpciClientTdispMutableState {
    fn update_tdi_state(&mut self, new_state: TdispTdiState) {
        tracing::info!(
            old_state = %self.tdi_state,
            new_state = %new_state,
            "updating TDI state based on host response"
        );
        self.tdi_state = new_state;
    }

    fn update_guest_device_id(&mut self, new_device_id: TdispDeviceId) {
        tracing::info!(
            old_device_id = ?self.guest_device_id,
            new_device_id = ?new_device_id,
            "updating guest device ID based on host response"
        );
        self.guest_device_id = new_device_id;
    }
}

/// TDISP state for a VPCI device.
#[derive(Inspect)]
pub struct VpciClientTdispState {
    #[inspect(skip)]
    worker_req: mesh::Sender<WorkerRequest>,
    // The device ID if the VPCI channel. Not to be confused with the guest device ID returned by the host in TDISP reports.
    vpci_device_id: u64,
    isolation_type: IsolationType,
    vtom: u64,
    #[inspect(debug)]
    target_vtl: Vtl,
    mutable_state: VpciClientTdispMutableState,
    /// Platform hooks used to gate attestation and unblock device resources.
    /// Required: a device driven through the TDISP flow must always have a
    /// validator, so that no platform silently skips validation. Platforms with
    /// nothing to do use `mocks::TdispNoopResourceValidator`.
    #[inspect(skip)]
    resource_validator: Arc<dyn TdispResourceValidationInterface>,
}

/// Manages the TDISP protocol for a TDISP-capable VPCI device.
impl VpciClientTdispState {
    pub(super) fn new(
        worker_req: mesh::Sender<WorkerRequest>,
        device_id: u64,
        resource_validator: Arc<dyn TdispResourceValidationInterface>,
        isolation_type: IsolationType,
        vtom: u64,
        target_vtl: Vtl,
    ) -> Self {
        Self {
            worker_req,
            vpci_device_id: device_id,
            mutable_state: VpciClientTdispMutableState {
                tdi_state: TdispTdiState::Unlocked,
                guest_device_id: TdispDeviceId::Invalid,
                validated_mmio_bars: std::collections::HashMap::new(),
                dma_unblocked: false,
                tdi_report: None,
                intercepted_bars: HashSet::new(),
            },
            isolation_type,
            vtom,
            target_vtl,
            resource_validator,
        }
    }

    /// Get the TDI state returned by the host for the most recent operation.
    fn tdi_state(&self) -> TdispTdiState {
        self.mutable_state.tdi_state
    }

    /// Require the TDI to be in `expected`, according to both the host and the
    /// platform firmware.
    ///
    /// # Panics
    ///
    /// Panics if either source reports anything other than `expected`.
    ///
    /// The host's reported state and the firmware's state are two independent
    /// answers to the same question, and at these points in the flow there is
    /// exactly one answer that is correct. Either source diverging from it
    /// means the paravisor's view of the device is wrong, which is not a
    /// condition it can recover from or safely continue past. A platform that
    /// cannot report its own state leaves only the host's answer to check.
    ///
    /// * `expected` - The state the TDI must be in.
    /// * `device_id` - Identifies the TDI device (not a VPCI ID). When no TDI
    ///   has been identified there is nothing to ask the firmware about, and
    ///   only the host's answer is checked.
    fn require_tdi_state(
        &self,
        expected: TdispTdiState,
        device_id: TdispDeviceId,
    ) -> anyhow::Result<()> {
        let cached = self.tdi_state();

        // Read the firmware first even when the host's answer is already wrong,
        // so the panic can report both values.
        let firmware = match device_id.id() {
            Some(device_id) => self
                .resource_validator
                .get_tsm_tdi_state(self.target_vtl, device_id)
                .context("require_tdi_state: failed to read the TDI state from the firmware")?,
            None => None,
        };

        if cached != expected {
            panic!(
                "TDI {device_id:?} must be in state {expected}, but the host reports \
                 {cached} (firmware reports {firmware:?})"
            );
        }

        match firmware {
            Some(firmware) if firmware != expected => {
                panic!(
                    "TDI {device_id:?} must be in state {expected}, but the firmware \
                     reports {firmware} (the host reports {cached})"
                );
            }
            Some(firmware) => tracing::trace!(
                ?device_id,
                %firmware,
                %expected,
                "require_tdi_state: host and firmware both confirm the TDI state"
            ),
            None => tracing::debug!(
                ?device_id,
                %cached,
                %expected,
                "require_tdi_state: firmware state unavailable, \
                 checking the host's answer alone"
            ),
        }

        Ok(())
    }

    pub(super) async fn send_tdisp_command(
        &mut self,
        payload: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse> {
        let serialized = openhcl_tdisp::serialize_command(&payload);

        // Ensure that the length does not exceed the VMBUS maximum packet size.
        // This shouldn't be possible since the host should reject the command anyways,
        // but fail earlier for safety.
        if serialized.len() > MAX_VPCI_TDISP_COMMAND_SIZE {
            return Err(anyhow::anyhow!(
                "serialized TDISP command exceeds VMBUS maximum packet size ({} > {})",
                serialized.len(),
                MAX_VPCI_TDISP_COMMAND_SIZE
            ));
        }

        // Make a mesh call to send the VMBUS packet to the host and await a response
        // packet from the host.
        let res = self
            .worker_req
            .call_failable(
                WorkerRequest::TdispCommand,
                vpci_protocol::VpciTdispCommand {
                    header: vpci_protocol::VpciTdispCommandHeader {
                        message_type: vpci_protocol::MessageType::VPCI_TDISP_COMMAND,
                        slot: SlotNumber::from_bits(self.vpci_device_id as u32),
                        data_length: serialized.len() as u64,
                    },
                    data: serialized,
                },
            )
            .await
            .map_err(|err: mesh::rpc::RpcError<mesh::error::RemoteError>| {
                tracing::error!(
                    error = &err as &dyn std::error::Error,
                    "failed to send tdisp command"
                );
                anyhow::anyhow!("failed to send tdisp command")
            })?;

        // Record state transitions based on the TDI state returned by the host in the response, if available.
        match res.tdi_state_after_enum() {
            Some(state) => self.mutable_state.update_tdi_state(state),
            None => tracing::warn!("host did not return valid TDI state in response"),
        }

        match res.error_code() {
            Some(TdispGuestOperationErrorCode::Success) => Ok(res),
            other => {
                let err_name = match other {
                    Some(code) => format!("{code:?}"),
                    None => format!("Unknown({})", res.result),
                };
                let err_msg = format!(
                    "send_tdisp_command {:?} failed because host responded with an error: {}",
                    payload.type_name(),
                    err_name,
                );

                tracing::error!(msg = err_msg);
                Err(anyhow::anyhow!(err_msg))
            }
        }
    }

    /// Get the TDISP interface info for the device, negotiating the given
    /// guest protocol with the host.
    ///
    /// * `target_protocol` - The guest protocol to negotiate.
    pub async fn tdisp_get_device_interface_info(
        &mut self,
        target_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        let res = self
            .send_tdisp_command(openhcl_tdisp::new_get_device_interface_info_command(
                self.vpci_device_id,
                target_protocol,
            ))
            .await?;

        match res.response::<TdispCommandResponseGetDeviceInterfaceInfo>() {
            Ok(info) => info.interface_info.ok_or_else(|| {
                anyhow::anyhow!("missing interface_info after validation, this should never happen")
            }),
            Err(err) => Err(anyhow::anyhow!(
                "error response in get_device_interface_info: {err}"
            )),
        }
    }

    /// Bind the device to the current partition, transitioning the TDI from
    /// Unlocked to Locked.
    ///
    /// While Locked the device can still perform unencrypted operations. The
    /// state exists to keep the device from modifying its resources between
    /// the bind and attestation.
    pub async fn tdisp_bind_interface(&mut self) -> anyhow::Result<()> {
        let state_before = self.tdi_state();
        let res = self
            .send_tdisp_command(openhcl_tdisp::new_bind_command(self.vpci_device_id))
            .await?;

        // The host should have transitioned the device to the Bind state if the bind was successful.
        match self.tdi_state() {
            TdispTdiState::Locked => {
                tracing::info!("device successfully transitioned to Bind state after bind command")
            }
            state_after => {
                tracing::error!(
                    %state_before,
                    state_after = %state_after,
                    "device is in unexpected TDI state after bind command, expected Locked"
                );
                anyhow::bail!(
                    "device is in unexpected TDI state after bind command, expected Locked"
                );
            }
        }

        match res.response::<TdispCommandResponseBind>() {
            Ok(_) => Ok(()),
            Err(err) => Err(anyhow::anyhow!(
                "error response in tdisp_bind_interface: {err}"
            )),
        }
    }

    /// Start a bound device, transitioning the TDI from Locked to Run. This is
    /// the point from which resources can be accepted into the guest context.
    pub async fn tdisp_start_device(&mut self) -> anyhow::Result<()> {
        let state_before = self.tdi_state();
        let res = self
            .send_tdisp_command(openhcl_tdisp::new_start_tdi_command(self.vpci_device_id))
            .await?;

        match self.tdi_state() {
            TdispTdiState::Run => {
                tracing::info!("device successfully transitioned to Run state after start command")
            }
            state_after => {
                tracing::error!(
                    %state_before,
                    state_after = %state_after,
                    "device is in unexpected TDI state after start command, expected Run"
                );
                anyhow::bail!(
                    "device is in unexpected TDI state after start command, expected Run"
                );
            }
        }

        match res.response::<TdispCommandResponseStartTdi>() {
            Ok(_) => Ok(()),
            Err(err) => Err(anyhow::anyhow!(
                "error response in tdisp_start_device: {err}"
            )),
        }
    }

    /// Request a report from the TDI or the physical device, as raw bytes.
    ///
    /// * `report_type` - Selects which report to fetch, which also determines
    ///   whether the TDI must be Locked or Run to ask for it.
    pub async fn tdisp_get_device_report(
        &mut self,
        report_type: &TdispReportType,
    ) -> anyhow::Result<Vec<u8>> {
        let res = self
            .send_tdisp_command(openhcl_tdisp::new_get_tdi_report_command(
                self.vpci_device_id,
                *report_type,
            ))
            .await?;

        match res.response::<TdispCommandResponseGetTdiReport>() {
            Ok(r) => Ok(r.report_buffer),
            Err(err) => Err(anyhow::anyhow!(
                "error response in tdisp_get_device_report: {err}"
            )),
        }
    }

    /// Fetch the device's TDI interface report and decode it. The report
    /// describes the TDI's MMIO ranges and their TEE/non-TEE attributes, which
    /// is what decides each BAR's isolation.
    pub async fn tdisp_get_tdi_report(&mut self) -> anyhow::Result<TdiReportStruct> {
        let buffer = self
            .tdisp_get_device_report(&TdispReportType::InterfaceReport)
            .await
            .context("failed to get TDI report")?;

        // Log the raw bytes before parsing them, so a report that fails to
        // deserialize can still be decoded by hand from the trace.
        tracing::info!(
            vpci_device_id = self.vpci_device_id,
            len = buffer.len(),
            raw = format_args!("{buffer:02x?}"),
            "tdisp_get_tdi_report: raw TDI interface report from the host"
        );

        let report = tdisp::devicereport::deserialize_tdi_report(&buffer)
            .context("failed to deserialize TDI report from host")?;

        tracing::info!(
            vpci_device_id = self.vpci_device_id,
            ?report,
            "tdisp_get_tdi_report: decoded TDI interface report"
        );

        // Break the MMIO ranges out individually: these decide each BAR's
        // PRIVATE/SHARED classification and whether it is auto-marked
        // intercepted, so they are what needs reading at a glance.
        for range in &report.mmio_interface_info {
            tracing::info!(
                "tdisp_get_tdi_report: MMIO range: range_id={}, first_4k_page_offset={:#x}, \
                 num_4k_pages={}, size_bytes={:#x}, is_non_tee_mem={}, \
                 is_mem_attr_updatable={}, range_maps_msix_table={}, range_maps_msix_pba={}",
                range.range_id,
                range.first_4k_page_offset,
                range.num_4k_pages,
                u64::from(range.num_4k_pages) * 4096,
                range.flags.is_non_tee_mem(),
                range.flags.is_mem_attr_updatable(),
                range.flags.range_maps_msix_table(),
                range.flags.range_maps_msix_pba()
            );
        }

        Ok(report)
    }

    /// Fetch the device's TDI device id, which identifies the TDI in platform
    /// calls. Available in any TDI state, unlike the other reports.
    pub async fn tdisp_get_tdi_device_id(&mut self) -> anyhow::Result<u64> {
        let buffer = self
            .tdisp_get_device_report(&TdispReportType::GuestDeviceId)
            .await
            .context("failed to get TDI device ID")?;

        // Ensure it's a u64
        if buffer.len() != size_of::<u64>() {
            return Err(anyhow::anyhow!("unexpected buffer size for TDI device ID"));
        }

        Ok(u64::from_le_bytes(buffer.try_into().unwrap()))
    }

    /// Tell the host to block an MMIO range, reversing a previous unblock.
    /// This only notifies the host; the platform-side block is separate.
    ///
    /// * `range_id` - Identifies which MMIO range to block (the PCI BAR index).
    /// * `gpa_base` - The guest physical base address of the range.
    /// * `range_len_bytes` - The length of the range, in bytes.
    pub async fn tdisp_host_block_mmio_range(
        &mut self,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        self.send_modify_mmio_range(
            openhcl_tdisp::new_block_mmio_range_command(
                self.vpci_device_id,
                range_id,
                gpa_base,
                range_len_bytes,
            ),
            "tdisp_host_block_mmio_range",
            range_id,
            gpa_base,
            range_len_bytes,
        )
        .await
    }

    /// Tell the host to unblock an MMIO range, so its view matches the
    /// platform's. This only notifies the host; the platform-side unblock is
    /// separate.
    ///
    /// * `range_id` - Identifies which MMIO range to unblock (the PCI BAR
    ///   index).
    /// * `gpa_base` - The guest physical base address of the range.
    /// * `range_len_bytes` - The length of the range, in bytes.
    pub async fn tdisp_host_unblock_mmio_range(
        &mut self,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        self.send_modify_mmio_range(
            openhcl_tdisp::new_unblock_mmio_range_command(
                self.vpci_device_id,
                range_id,
                gpa_base,
                range_len_bytes,
            ),
            "tdisp_host_unblock_mmio_range",
            range_id,
            gpa_base,
            range_len_bytes,
        )
        .await
    }

    /// Sends a `ModifyMmioRange` command, for either action.
    ///
    /// * `command` - The command to send.
    /// * `caller` - Names the operation in the trace and error output.
    /// * `range_id`, `gpa_base`, `range_len_bytes` - The range the command
    ///   describes, passed separately so it can be logged without decoding
    ///   the built command.
    async fn send_modify_mmio_range(
        &mut self,
        command: GuestToHostCommand,
        caller: &str,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        tracing::info!(
            "sending ModifyMmioRange to the host: range_id={range_id}, gpa_base={gpa_base:#x}, range_len_bytes={range_len_bytes:#x}"
        );

        let res = self.send_tdisp_command(command).await?;

        // The command requires the TDI to be Locked or Run, so record what the
        // host thought the state was: an InvalidDeviceState response is most
        // easily explained by this pair.
        let tdi_state_before = res.tdi_state_before_enum();
        let tdi_state_after = res.tdi_state_after_enum();

        // Unlike bind and start, this command does not transition the TDI, so
        // there is no post-command state to check.
        match res.response::<TdispCommandResponseModifyMmioRange>() {
            Ok(_) => {
                tracing::info!(
                    "host accepted ModifyMmioRange: caller={caller}, range_id={range_id}, gpa_base={gpa_base:#x}, range_len_bytes={range_len_bytes:#x}, tdi_state_before={tdi_state_before:?}, tdi_state_after={tdi_state_after:?}",
                );
                Ok(())
            }
            Err(err) => {
                tracing::error!(
                    "host rejected ModifyMmioRange: caller={caller}, range_id={range_id}, gpa_base={gpa_base:#x}, range_len_bytes={range_len_bytes:#x}, tdi_state_before={tdi_state_before:?}, tdi_state_after={tdi_state_after:?}, error={err}",
                );
                Err(anyhow::anyhow!("error response in {caller}: {err}"))
            }
        }
    }

    /// Unbind the device, returning the TDI to Unlocked and dropping all
    /// per-attest state so the next attestation starts clean.
    ///
    /// Any resource still unblocked is flipped back to shared first. That part
    /// is best-effort: a failure is logged but does not abort the unbind.
    ///
    /// * `reason` - Reported to the host to explain why the TDI is unbinding.
    pub async fn tdisp_unbind(&mut self, reason: TdispGuestUnbindReason) -> anyhow::Result<()> {
        // Flip all unblocked MMIO ranges and DMA back to shared before we tell
        // the host to unbind the TDI. This is best-effort: a failure here is
        // logged but doesn't abort the unbind. A new attestation won't proceed
        // if all resources were not successfully torn down.
        let validator = self.resource_validator.clone();
        let device_id = self.mutable_state.guest_device_id;

        // The teardown below addresses a real TDI through the platform. Without
        // an id there is nothing attested to tear down: no range was ever
        // unblocked, DMA was never unblocked, and no report was ever recorded.
        if let Some(raw_device_id) = device_id.id() {
            let validated_bars_clone = self.mutable_state.validated_mmio_bars.clone();
            for (bar_id, mmio) in validated_bars_clone {
                match mmio.isolation {
                    // Nothing was ever unblocked for these, so there is nothing
                    // to block back.
                    TdispResourceIsolation::Shared | TdispResourceIsolation::Invalid => {
                        self.mutable_state.validated_mmio_bars.remove(&bar_id);
                        continue;
                    }
                    TdispResourceIsolation::Private => {}
                }

                let block_mmio_res = validator
                    .tdisp_block_mmio(
                        Vtl::Vtl2,
                        raw_device_id,
                        mmio.base_gpa,
                        0,
                        mmio.length_in_bytes,
                        bar_id,
                    )
                    .await;

                if let Err(e) = block_mmio_res {
                    tracing::error!(
                        bar_id,
                        base_gpa = format_args!("{:#x}", mmio.base_gpa),
                        length_in_bytes = mmio.length_in_bytes,
                        error = &*e as &dyn std::error::Error,
                        "tdisp_unbind: failed to re-block MMIO range"
                    );
                } else {
                    // Tell the host only once the platform actually blocked the
                    // range, so the host's view never runs ahead of the platform's.
                    // Best-effort, like the block above.
                    if let Err(e) = self
                        .tdisp_host_block_mmio_range(bar_id, mmio.base_gpa, mmio.length_in_bytes)
                        .await
                    {
                        tracing::error!(
                            bar_id,
                            base_gpa = format_args!("{:#x}", mmio.base_gpa),
                            length_in_bytes = mmio.length_in_bytes,
                            error = &*e as &dyn std::error::Error,
                            "tdisp_unbind: failed to block MMIO range on the host"
                        );
                    }

                    // Successful re-block, remove the bar from the validated list.
                    self.mutable_state.validated_mmio_bars.remove(&bar_id);
                }
            }

            if self.mutable_state.dma_unblocked {
                if let Err(e) = validator.tdisp_block_dma(Vtl::Vtl2, raw_device_id) {
                    tracing::error!(
                        raw_device_id,
                        error = &*e as &dyn std::error::Error,
                        "tdisp_unbind: failed to re-block DMA"
                    );
                } else {
                    // Successful re-block, clear the DMA unblocked flag.
                    self.mutable_state.dma_unblocked = false;
                }
            }

            self.resource_validator
                .tdisp_clear_tdi_report(raw_device_id);
        }

        // Clear every per-attest field. All of these will be fetched cleanly on
        // the next re-attest cycle.
        self.mutable_state.tdi_report = None;
        self.mutable_state.guest_device_id = TdispDeviceId::Invalid;
        self.mutable_state.intercepted_bars.clear();

        let res = self
            .send_tdisp_command(openhcl_tdisp::new_unbind_command(
                self.vpci_device_id,
                reason,
            ))
            .await?;

        if let Err(err) = res.response::<TdispCommandResponseUnbind>() {
            return Err(anyhow::anyhow!("error response in tdisp_unbind: {err}"));
        }

        // The TDI must be back in Unlocked, and the firmware has to agree that
        // it actually came back rather than the host merely saying so.
        self.require_tdi_state(TdispTdiState::Unlocked, device_id)?;

        Ok(())
    }

    /// Detects TDISP capabilities for the device. If the device supports TDISP
    /// and a guest protocol type that we support given the current VM's
    /// isolation level, then returns the interface info. Otherwise, returns an
    /// error representing why the device is not suitable for TDISP.
    ///
    /// The result is not retained, so each call queries the device afresh.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    pub async fn query_capabilities(&mut self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        tracing::info!(
            ?self.isolation_type,
            "querying TDISP capabilities for device given VM isolation type"
        );

        let target_protocol = match self.isolation_type {
            IsolationType::Snp => TdispGuestProtocolType::AmdSevTioV1,
            IsolationType::Tdx => TdispGuestProtocolType::IntelTdxConnectV1,
            IsolationType::Vbs => {
                tracing::warn!(
                    "query_capabilities: VM is running with VBS isolation (NOT SUPPORTED)"
                );
                anyhow::bail!("VBS isolation is not currently supported for TDISP")
            }
            IsolationType::Cca => {
                tracing::warn!(
                    "query_capabilities: VM is running with CCA isolation (NOT SUPPORTED)"
                );
                anyhow::bail!("CCA isolation is not currently supported for TDISP")
            }
            IsolationType::None => {
                tracing::warn!("query_capabilities: VM is running with no isolation (no TDISP)");
                anyhow::bail!("TDISP is not supported without isolation")
            }
        };

        let device_interface_info = self
            .tdisp_get_device_interface_info(target_protocol)
            .await
            .context("tdisp_query_capabilities: failed to get device interface info")?;

        tracing::info!(
            ?device_interface_info,
            "tdisp_query_capabilities: device interface info",
        );

        if device_interface_info.guest_protocol_type == target_protocol.into() {
            tracing::info!(
                ?device_interface_info.guest_protocol_type,
                "tdisp_query_capabilities: TDISP is supported",
            );

            Ok(device_interface_info)
        } else {
            tracing::info!(
                ?device_interface_info.guest_protocol_type,
                ?target_protocol,
                "tdisp_query_capabilities: device does not support a guest protocol we support",
            );

            anyhow::bail!("device does not support expected guest protocol we support");
        }
    }

    #[cfg(not(feature = "dev_snp_ohcl_tio_support"))]
    /// Always fails: TDISP support was not compiled in.
    pub async fn query_capabilities(&mut self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        anyhow::bail!("TDISP feature not enabled during compile time")
    }

    /// Run the full attestation flow, leaving the TDI in Run with its interface
    /// report cached. Any prior attestation is torn down first, so this is safe
    /// to call from any TDI state.
    ///
    /// Resources are not yet accessible on return. They are unblocked when the
    /// guest enables MMIO, so that platform validation runs against the
    /// addresses the guest actually programmed.
    ///
    /// * `interface_info` - The negotiated capabilities for this device.
    pub async fn attest(&mut self, interface_info: TdispDeviceInterfaceInfo) -> anyhow::Result<()> {
        tracing::info!(
            ?interface_info,
            "tdisp_attest_device: beginning attestation flow"
        );

        // If there are any existing attestation artifacts, we need to clear
        // them before starting a new attestation.
        if self.mutable_state.dma_unblocked || !self.mutable_state.validated_mmio_bars.is_empty() {
            tracing::info!(
                current_state = %self.tdi_state(),
                "tdisp_attest_device: TDI not in Unlocked, unbinding before rebind"
            );
            self.tdisp_unbind(TdispGuestUnbindReason::Graceful)
                .await
                .context("tdisp_attest_device: failed to unbind device from running state")?;
        }

        // If there are *still* any attestation artifacts after unbind,
        // something went wrong. We can't continue out of paranoia.
        if self.mutable_state.dma_unblocked || !self.mutable_state.validated_mmio_bars.is_empty() {
            anyhow::bail!(
                "tdisp_attest_device: failed to clear existing attestation state, cannot proceed with new attestation"
            );
        }

        // Request the guest device ID before binding so the pre-bind and
        // pre-start validator hooks can identify the TDI they are gating.
        let guest_device_id = self
            .tdisp_get_tdi_device_id()
            .await
            .context("tdisp_attest_device: failed to get TDI device ID before binding device")?;

        // Platforms require a u16 device ID even though the report returns a
        // u64. Ensure the returned device ID fits within that constraint before
        // proceeding.
        let guest_device_id_u16 = u16::try_from(guest_device_id)
            .context("tdisp_attest_device: guest device ID must fit within u16")?;

        self.resource_validator
            .on_pre_bind(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: pre-bind validation failed")?;

        self.tdisp_bind_interface()
            .await
            .context("tdisp_attest_device: failed to bind device interface")?;

        self.require_tdi_state(
            TdispTdiState::Locked,
            TdispDeviceId::Valid(guest_device_id_u16),
        )
        .context("tdisp_attest_device: failed to confirm the TDI is Locked after the bind")?;

        self.resource_validator
            .on_pre_start(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: pre-start validation failed")?;

        self.tdisp_start_device()
            .await
            .context("tdisp_attest_device: failed to start device")?;

        self.require_tdi_state(
            TdispTdiState::Run,
            TdispDeviceId::Valid(guest_device_id_u16),
        )
        .context("tdisp_attest_device: failed to confirm the TDI is in Run after the start")?;

        self.resource_validator
            .on_post_start(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: post-start validation failed")?;

        // Fetch and save the TDI interface report so callers can inspect the
        // attested device's reported capabilities and MMIO ranges.
        let tdi_report = self.tdisp_get_tdi_report().await.context(
            "tdisp_attest_device: failed to get TDI interface report after starting device",
        )?;

        tracing::info!(
            ?tdi_report,
            %guest_device_id,
            "tdisp_attest_device: device attestation flow completed successfully, waiting on resources to be assigned"
        );

        self.mutable_state
            .update_guest_device_id(TdispDeviceId::Valid(guest_device_id_u16));

        // Hand the report to the validator before any resource is unblocked.
        self.resource_validator
            .tdisp_set_tdi_report(guest_device_id_u16, &tdi_report);

        // Auto-mark any MMIO range that the device reports as mapping the MSI-X
        // table or PBA as intercepted. Intercepted BARs are not backed by RAM
        // on the host and therefore cannot be made private. Unblock calls will
        // be skipped for these BARs.
        for range in &tdi_report.mmio_interface_info {
            if range.flags.range_maps_msix_table() || range.flags.range_maps_msix_pba() {
                tracing::info!(
                    bar_id = range.range_id,
                    maps_msix_table = range.flags.range_maps_msix_table(),
                    maps_msix_pba = range.flags.range_maps_msix_pba(),
                    "auto-marking MSI-X table/PBA BAR as intercepted based on TDI report"
                );
                self.mutable_state.intercepted_bars.insert(range.range_id);
            }
        }

        self.mutable_state.tdi_report = Some(tdi_report);

        // Device is now in the Run state without resource validation being
        // performed.
        Ok(())
    }

    /// Get the TDI state of the device. This is used for testing and validation purposes, and is not part of the standard TDISP flow.
    pub fn tdisp_get_tdi_state(&self) -> TdispTdiState {
        self.tdi_state()
    }

    /// Mark a BAR as being intercepted by the host. The classic case is the
    /// MSI-X table / PBA BAR which is handled by the hypervisor through MMIO
    /// enlightenments.
    ///
    /// Such a BAR is never made private, because it is not backed by RAM on
    /// the host and so has nothing that could be flipped.
    ///
    /// * `bar_id` - The PCI BAR index to mark.
    pub fn mark_bar_intercepted(&mut self, bar_id: u16) {
        if self.mutable_state.intercepted_bars.insert(bar_id) {
            tracing::info!(
                bar_id,
                "marking BAR as intercepted; TDISP MMIO unblock will be skipped for this BAR"
            );
        }
    }

    /// Returns true if the given BAR has been marked intercepted.
    pub fn is_bar_intercepted(&self, bar_id: u16) -> bool {
        self.mutable_state.intercepted_bars.contains(&bar_id)
    }

    /// Classify a single BAR's isolation from the cached TDI interface report
    /// and the set of intercepted BARs.
    ///
    /// This is the single source of truth for the question, so that what is
    /// reported to the guest and what is actually unblocked cannot disagree:
    /// `Private` is exactly a BAR that gets unblocked, `Shared` is one that is
    /// deliberately skipped, and `Invalid` means the BAR has no entry in the
    /// report.
    ///
    /// * `bar_id` - The PCI BAR index to classify.
    fn classify_bar(&self, bar_id: u16) -> TdispResourceIsolation {
        // Host-intercepted BARs (MSI-X table / PBA) have no host-RAM
        // backing and can never be flipped private, so always SHARED,
        // independent of what the report says.
        if self.mutable_state.intercepted_bars.contains(&bar_id) {
            return TdispResourceIsolation::Shared;
        }

        // No cached report yet (attestation hasn't run) → we don't know
        // if this BAR is claimed at all, so INVALID rather than SHARED.
        let Some(report) = self.mutable_state.tdi_report.as_ref() else {
            return TdispResourceIsolation::Invalid;
        };

        // `range_id` == PCI BAR index for the guest protocols we
        // support. A missing entry is an unused slot or the upper half
        // of a 64-bit BAR (not reported independently).
        let Some(range) = report
            .mmio_interface_info
            .iter()
            .find(|r| r.range_id == bar_id)
        else {
            return TdispResourceIsolation::Invalid;
        };

        // `is_non_tee_mem` ranges have no protected backing and must
        // never be passed to `tdisp_unblock_mmio`. Report SHARED and
        // skip. Everything else is TEE memory the TDI owns → PRIVATE.
        if range.flags.is_non_tee_mem() {
            TdispResourceIsolation::Shared
        } else {
            TdispResourceIsolation::Private
        }
    }

    /// Classify BAR and DMA isolation for this device at this instant,
    /// suitable for populating a `VpciIsolatedResourcesReply` on the
    /// guest-facing side.
    ///
    /// This is pure classification over the currently cached state: it
    /// never drives attestation, so it only ever returns
    /// [`TdispIsolationReport::NotReady`] or
    /// [`TdispIsolationReport::Ready`]. `NotTdispCapable` and `Error`
    /// are decided by the callers above.
    ///
    /// Returns `NotReady` iff no TDI interface report is currently
    /// cached, which is the case both before the first attestation and
    /// after any unbind, since unbinding drops the report along with the
    /// rest of the per-attest state. Callers that need a classification
    /// from an unattested device have to attest it first.
    pub fn isolation_snapshot(&self) -> TdispIsolationReport {
        if self.mutable_state.tdi_report.is_none() {
            return TdispIsolationReport::NotReady;
        }

        let mut bars = [TdispResourceIsolation::Invalid; 6];
        for bar_id in 0..6u16 {
            bars[bar_id as usize] = self.classify_bar(bar_id);
        }

        let dma: TdispResourceIsolation = {
            // If any BAR is classified as PRIVATE, the the device should also have PRIVATE DMA.
            if bars.contains(&TdispResourceIsolation::Private) {
                // TDISP devices with private MMIO always have private DMA, even
                // if at this moment the device's DMA isn't unblocked.
                TdispResourceIsolation::Private
            } else {
                TdispResourceIsolation::Shared
            }
        };

        TdispIsolationReport::Ready { bars, dma }
    }

    /// Called when a BAR MMIO range is reconfigured by the guest, to make the
    /// range accessible to the guest if it is private memory.
    ///
    /// Only ranges the device reports as TEE memory are unblocked. Ranges the
    /// device reports as non-TEE memory, and BARs the paravisor has marked
    /// intercepted, are skipped: neither is protected memory, so there is
    /// nothing to flip. A BAR with no entry in the report at all is an error,
    /// since it means the device was never attested.
    ///
    /// Doing this on reconfiguration rather than at attestation time is what
    /// lets the platform validate against the addresses the guest actually
    /// programmed.
    ///
    /// # Arguments
    ///
    /// * `bar_id` - The BAR index being configured. Matched against the
    ///   `range_id` of the MMIO ranges reported in the TDI interface report.
    /// * `base_address` - The base guest physical address of the MMIO range.
    /// * `length` - The length in bytes of the MMIO range.
    pub async fn tdisp_on_mmio_reconfigured(
        &mut self,
        bar_id: u16,
        base_address: u64,
        length: u64,
    ) -> anyhow::Result<()> {
        // If the device is not attested and in Run state, don't attempt to unblock resources
        if self.tdi_state() != TdispTdiState::Run {
            tracing::warn!(
                bar_id,
                base_address,
                length,
                "ignoring MMIO reconfiguration callback because device is not in Run state"
            );
            return Ok(());
        }

        if self.mutable_state.validated_mmio_bars.contains_key(&bar_id) {
            tracing::debug!(
                bar_id,
                "skipping MMIO unblock for BAR that has already been validated"
            );
            return Ok(());
        }

        match self.classify_bar(bar_id) {
            TdispResourceIsolation::Shared => {
                tracing::info!(
                    bar_id,
                    base_address,
                    length,
                    "skipping MMIO unblock for BAR classified SHARED \
                     (intercepted or non-TEE memory)"
                );
                // Record the range so we don't repeatedly fall through here
                // on subsequent reconfigurations. Marked `Shared`, which
                // tells the unbind path there is no block call to undo.
                self.mutable_state.validated_mmio_bars.insert(
                    bar_id,
                    ValidatedMmio {
                        base_gpa: base_address,
                        length_in_bytes: length,
                        isolation: TdispResourceIsolation::Shared,
                    },
                );
                return Ok(());
            }
            TdispResourceIsolation::Invalid => {
                anyhow::bail!(
                    "tdisp_on_mmio_reconfigured: BAR {bar_id} has no entry in \
                     the TDI interface report (or report not available); \
                     device has not been attested"
                );
            }
            TdispResourceIsolation::Private => {}
        }

        // A BAR only classifies Private once the interface report is cached,
        // which happens during attestation alongside the device id, so reaching
        // here without one means the two have gone out of step.
        let Some(device_id) = self.mutable_state.guest_device_id.id() else {
            anyhow::bail!(
                "tdisp_on_mmio_reconfigured: BAR {bar_id} is classified Private but no \
                 TDI device id is known"
            );
        };
        let host = VpciTdispHostSender {
            worker_req: self.worker_req.clone(),
            vpci_device_id: self.vpci_device_id,
        };

        tracing::info!(
            "tdisp_on_mmio_reconfigured: unblocking MMIO for BAR classified PRIVATE: \
             device_id={device_id:#x}, bar_id={bar_id}, base_address={base_address:#x}, \
             length={length:#x}"
        );

        self.resource_validator
            .tdisp_unblock_mmio(
                self.target_vtl,
                device_id,
                base_address,
                0,
                length,
                bar_id,
                &host,
            )
            .await
            .context("tdisp_on_mmio_reconfigured: failed to unblock MMIO")?;

        tracing::info!(
            "tdisp_on_mmio_reconfigured: MMIO unblocked: device_id={device_id:#x}, \
             bar_id={bar_id}, base_address={base_address:#x}, length={length:#x}"
        );

        self.mutable_state.validated_mmio_bars.insert(
            bar_id,
            ValidatedMmio {
                base_gpa: base_address,
                length_in_bytes: length,
                isolation: TdispResourceIsolation::Private,
            },
        );

        // After the first successful MMIO unblock following attestation,
        // unblock DMA as well so the device can issue DMA traffic to the
        // guest. Guard with `dma_unblocked` so it only fires once per
        // bind/attest cycle (cleared on unbind).
        if !self.mutable_state.dma_unblocked {
            tracing::info!("tdisp_on_mmio_reconfigured: unblocking DMA: device_id={device_id:#x}");

            self.resource_validator
                .tdisp_unblock_dma(self.target_vtl, device_id)
                .context("tdisp_on_mmio_reconfigured: failed to unblock DMA")?;
            self.mutable_state.dma_unblocked = true;
            tracing::info!("tdisp_on_mmio_reconfigured: DMA unblocked: device_id={device_id:#x}");
        } else {
            tracing::info!(
                "tdisp_on_mmio_reconfigured: skipping DMA unblock, already unblocked this \
                 bind/attest cycle: device_id={device_id:#x}"
            );
        }

        Ok(())
    }
}

impl TdispVirtualDeviceInterface for VpciDevice {
    async fn send_tdisp_command(
        &self,
        payload: GuestToHostCommand,
    ) -> Result<GuestToHostResponse, anyhow::Error> {
        let mut guard = self.tdisp.0.lock().await;
        guard.send_tdisp_command(payload).await
    }

    async fn tdisp_get_device_interface_info(
        &self,
        target_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_get_device_interface_info(target_protocol).await
    }

    async fn tdisp_bind_interface(&self) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_bind_interface().await
    }

    async fn tdisp_start_device(&self) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_start_device().await
    }

    async fn tdisp_get_device_report(
        &self,
        report_type: &TdispReportType,
    ) -> anyhow::Result<Vec<u8>> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_get_device_report(report_type).await
    }

    async fn tdisp_get_tdi_report(&self) -> anyhow::Result<TdiReportStruct> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_get_tdi_report().await
    }

    async fn tdisp_get_tdi_device_id(&self) -> anyhow::Result<u64> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_get_tdi_device_id().await
    }

    async fn tdisp_unbind(&self, reason: TdispGuestUnbindReason) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_unbind(reason).await
    }

    async fn tdisp_host_block_mmio_range(
        &self,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard
            .tdisp_host_block_mmio_range(range_id, gpa_base, range_len_bytes)
            .await
    }

    async fn tdisp_host_unblock_mmio_range(
        &self,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard
            .tdisp_host_unblock_mmio_range(range_id, gpa_base, range_len_bytes)
            .await
    }
}

/// Higher level interface for TDISP operations on a VPCI device.
#[expect(async_fn_in_trait)]
pub trait TdispVpciAttestationInterface: Sync + Send {
    /// Attests the device using the TDISP flow. This includes binding the
    /// device, starting it, and any other validation steps on reports that are
    /// necessary for the device to be considered attested.
    ///
    /// The whole flow is performed atomically from the caller's point of view:
    /// on success (`Ok`) the device is left in Run, and on failure (`Err`) it
    /// is left Unlocked with no attestation state retained.
    ///
    /// Device resources are not yet accessible on return. They are unblocked
    /// later, when the guest enables MMIO.
    ///
    /// * `interface_info` - The negotiated capabilities for this device.
    async fn tdisp_attest_device(
        &self,
        interface_info: TdispDeviceInterfaceInfo,
    ) -> anyhow::Result<()>;

    /// Detects TDISP capabilities for the device. If the device supports TDISP
    /// and a guest protocol type that we support given the current VM's
    /// isolation level, then returns the interface info. Otherwise, returns an
    /// error representing why the device is not suitable for TDISP.
    async fn tdisp_query_capabilities(&self) -> anyhow::Result<TdispDeviceInterfaceInfo>;

    /// Get the TDI state of the device.
    async fn tdisp_tdi_state(&self) -> TdispTdiState;

    /// Called when a BAR MMIO range is reconfigured by the guest, to make the
    /// range accessible to the guest if it is private memory.
    ///
    /// # Arguments
    ///
    /// * `bar_id` - The BAR index being configured.
    /// * `base_address` - The base guest physical address of the MMIO range.
    /// * `length` - The length in bytes of the MMIO range.
    async fn tdisp_on_mmio_reconfigured(
        &self,
        bar_id: u16,
        base_address: u64,
        length: u64,
    ) -> anyhow::Result<()>;

    /// Mark a BAR as paravisor-intercepted, so that it is never made private
    /// on MMIO reconfiguration. Use this for BARs whose memory is registered
    /// as a paravisor MMIO intercept region (e.g. the MSI-X table / PBA BAR)
    /// and therefore has no host-side RAM backing that could be flipped.
    ///
    /// * `bar_id` - The PCI BAR index to mark.
    async fn tdisp_mark_bar_intercepted(&self, bar_id: u16);
}

impl TdispVpciAttestationInterface for VpciDevice {
    async fn tdisp_attest_device(
        &self,
        interface_info: TdispDeviceInterfaceInfo,
    ) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard.attest(interface_info).await
    }

    async fn tdisp_query_capabilities(&self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        let mut guard = self.tdisp.0.lock().await;
        guard.query_capabilities().await
    }

    async fn tdisp_tdi_state(&self) -> TdispTdiState {
        let guard = self.tdisp.0.lock().await;
        guard.tdi_state()
    }

    async fn tdisp_on_mmio_reconfigured(
        &self,
        bar_id: u16,
        base_address: u64,
        length: u64,
    ) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard
            .tdisp_on_mmio_reconfigured(bar_id, base_address, length)
            .await
    }

    async fn tdisp_mark_bar_intercepted(&self, bar_id: u16) {
        let mut guard = self.tdisp.0.lock().await;
        guard.mark_bar_intercepted(bar_id);
    }
}

impl VpciDevice {
    /// Return a classification of BAR and DMA isolation for this device,
    /// suitable for answering `VPCI_QUERY_ISOLATED_RESOURCES` on the
    /// guest-facing VPCI channel.
    ///
    /// An unattested device has no interface report to classify, so this
    /// attests it first to produce one. The whole sequence runs under a single
    /// hold of the per-device TDISP mutex, so the state observed here cannot
    /// change before it is acted on.
    pub async fn tdisp_isolation_snapshot(&self) -> TdispIsolationReport {
        let mut guard = self.tdisp.0.lock().await;

        if guard.tdi_state() == TdispTdiState::Unlocked {
            let info = match guard.query_capabilities().await {
                Ok(info) => info,
                Err(err) => {
                    tracing::error!(
                        "tdisp_isolation_snapshot: query_capabilities failed (tdisp not supported or host errored out): {err}"
                    );
                    return TdispIsolationReport::NotTdispCapable;
                }
            };
            if let Err(err) = guard.attest(info).await {
                tracing::error!(
                    error = &*err as &dyn std::error::Error,
                    "tdisp_isolation_snapshot: attest from Unlocked failed",
                );
                return TdispIsolationReport::Error;
            }
        }

        guard.isolation_snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tdisp::devicereport::TdispTdiReportInterfaceInfo;
    use tdisp::devicereport::TdispTdiReportMmioFlags;
    use tdisp::devicereport::TdispTdiReportMmioInterfaceInfo;

    /// Build a `VpciClientTdispState` with default fields and a dangling
    /// worker sender. `send_tdisp_command` must not be called on the
    /// returned value, but `isolation_snapshot` and the mutable-state
    /// fields it inspects are safe to poke directly.
    fn new_state() -> VpciClientTdispState {
        let (worker_req, _worker_recv) = mesh::channel::<WorkerRequest>();
        VpciClientTdispState::new(
            worker_req,
            /* device_id = */ 0,
            /* resource_validator = */
            Arc::new(openhcl_tdisp::mocks::TdispNoopResourceValidator::new()),
            IsolationType::None,
            /* vtom = */ 0,
            Vtl::Vtl0,
        )
    }

    /// Build a minimal `TdiReportStruct` containing only the given
    /// `mmio_interface_info` ranges, enough for `isolation_snapshot`.
    fn make_report(ranges: Vec<TdispTdiReportMmioInterfaceInfo>) -> TdiReportStruct {
        TdiReportStruct {
            interface_info: TdispTdiReportInterfaceInfo::new(),
            msi_x_message_control: 0,
            lnr_control: 0,
            tph_control: 0,
            mmio_interface_info: ranges,
        }
    }

    fn tee_range(range_id: u16) -> TdispTdiReportMmioInterfaceInfo {
        TdispTdiReportMmioInterfaceInfo {
            first_4k_page_offset: 0,
            num_4k_pages: 1,
            flags: TdispTdiReportMmioFlags::new().with_is_non_tee_mem(false),
            range_id,
        }
    }

    fn non_tee_range(range_id: u16) -> TdispTdiReportMmioInterfaceInfo {
        TdispTdiReportMmioInterfaceInfo {
            first_4k_page_offset: 0,
            num_4k_pages: 1,
            flags: TdispTdiReportMmioFlags::new().with_is_non_tee_mem(true),
            range_id,
        }
    }

    #[test]
    fn isolation_snapshot_not_ready_without_report() {
        // No cached TDI report → NotReady, regardless of TDI state.
        let state = new_state();
        assert!(matches!(
            state.isolation_snapshot(),
            TdispIsolationReport::NotReady
        ));

        let mut state = new_state();
        state.mutable_state.tdi_state = TdispTdiState::Run;
        assert!(matches!(
            state.isolation_snapshot(),
            TdispIsolationReport::NotReady
        ));
    }

    #[test]
    fn isolation_snapshot_ready_with_empty_report() {
        // Cached (empty) report → Ready; every BAR INVALID, DMA SHARED.
        // No TDI-state requirement.
        let mut state = new_state();
        state.mutable_state.tdi_report = Some(make_report(vec![]));
        let TdispIsolationReport::Ready { bars, dma } = state.isolation_snapshot() else {
            panic!("expected Ready");
        };
        assert_eq!(bars, [TdispResourceIsolation::Invalid; 6]);
        assert_eq!(dma, TdispResourceIsolation::Shared);
    }

    #[test]
    fn isolation_snapshot_classifies_report_ranges() {
        // BAR 0: TEE memory → PRIVATE.
        // BAR 2: non-TEE memory → SHARED.
        // BAR 4: TEE memory but intercepted → SHARED.
        // BARs 1, 3, 5: no entry → INVALID.
        let mut state = new_state();
        state.mutable_state.intercepted_bars.insert(4);
        state.mutable_state.tdi_report = Some(make_report(vec![
            tee_range(0),
            non_tee_range(2),
            tee_range(4),
        ]));
        let TdispIsolationReport::Ready { bars, dma } = state.isolation_snapshot() else {
            panic!("expected Ready");
        };
        assert_eq!(
            bars,
            [
                TdispResourceIsolation::Private,
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Shared,
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Shared,
                TdispResourceIsolation::Invalid,
            ]
        );
        assert_eq!(dma, TdispResourceIsolation::Private);
    }

    #[test]
    fn isolation_snapshot_dma_private_with_any_private_mmio() {
        let mut state = new_state();
        state.mutable_state.tdi_report = Some(make_report(vec![tee_range(0)]));
        let TdispIsolationReport::Ready { bars, dma } = state.isolation_snapshot() else {
            panic!("expected Ready");
        };
        assert_eq!(
            bars,
            [
                TdispResourceIsolation::Private,
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Invalid,
            ]
        );
        assert_eq!(dma, TdispResourceIsolation::Private);
    }
}
