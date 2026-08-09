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
use openhcl_tdisp::TdispCommandResponseStartTdi;
use openhcl_tdisp::TdispCommandResponseUnbind;
use openhcl_tdisp::TdispDeviceInterfaceInfo;
use openhcl_tdisp::TdispGuestOperationErrorCode;
use openhcl_tdisp::TdispGuestProtocolType;
use openhcl_tdisp::TdispGuestUnbindReason;
use openhcl_tdisp::TdispReportType;
use openhcl_tdisp::TdispVirtualDeviceInterface;
use tdisp::TdispTdiState;
use tdisp::devicereport::TdiReportStruct;
use virt::IsolationType;
use vpci_protocol::MAX_VPCI_TDISP_COMMAND_SIZE;
use vpci_protocol::ResourceIsolation;
use vpci_protocol::SlotNumber;

use super::VpciDevice;
use super::WorkerRequest;
use openhcl_tdisp::TdispResourceValidationInterface;
use std::collections::HashSet;
use std::sync::Arc;

/// Point-in-time classification of a device's BAR and DMA isolation.
///
/// Returned by [`VpciClientTdispState::isolation_snapshot`] and used to
/// populate `VpciIsolatedResourcesReply` on the paravisor's guest-facing
/// VPCI channel.
#[derive(Debug, Clone, Copy)]
pub enum IsolationSnapshot {
    /// The TDI is not in the `Run` state, or is in `Run` but no resource
    /// has been unblocked yet. The paravisor cannot answer the isolation
    /// query in this state. Callers should map this to an error reply.
    NotReady,
    /// The TDI is in `Run`. BAR entries may be `SHARED`, `PRIVATE`, or
    /// `INVALID` (for BAR IDs outside the device's known range, e.g. upper
    /// halves of 64-bit BARs). `dma` is always `SHARED` or `PRIVATE`.
    Ready {
        /// Classification for each of the device's six BARs.
        bars: [ResourceIsolation; 6],
        /// Classification for the device's DMA path.
        dma: ResourceIsolation,
    },
}

#[derive(Inspect)]
struct VpciClientTdispMutableState {
    tdi_state: TdispTdiState,
    guest_device_id: u16,
    /// Map of BAR ID to the `(base_gpa, length_in_bytes)` that was passed
    /// to `tdisp_unblock_mmio`. Populated on unblock and used during
    /// unbind to call `tdisp_block_mmio` with the same parameters so
    /// private pages can be flipped back to shared. Cleared on unbind.
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

/// Tracks the parameters used to unblock a BAR's MMIO pages, so the same
/// range can be re-blocked on unbind.
#[derive(Inspect, Clone, Copy, Debug)]
struct ValidatedMmio {
    #[inspect(hex)]
    base_gpa: u64,
    #[inspect(hex)]
    length_in_bytes: u32,
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

    fn update_guest_device_id(&mut self, new_device_id: u16) {
        tracing::info!(
            old_device_id = self.guest_device_id,
            new_device_id = new_device_id,
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
                guest_device_id: 0,
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

    /// See: [`TdispVirtualDeviceInterface::tdisp_get_device_interface_info`]
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

    /// See: [`TdispVirtualDeviceInterface::tdisp_bind_interface`]
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

    /// See: [`TdispVirtualDeviceInterface::tdisp_start_device`]
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

    /// See: [`TdispVirtualDeviceInterface::tdisp_get_device_report`]
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

    /// See: [`TdispVirtualDeviceInterface::tdisp_get_tdi_report`]
    pub async fn tdisp_get_tdi_report(&mut self) -> anyhow::Result<TdiReportStruct> {
        let buffer = self
            .tdisp_get_device_report(&TdispReportType::InterfaceReport)
            .await
            .context("failed to get TDI report")?;

        tdisp::devicereport::deserialize_tdi_report(&buffer)
            .context("failed to deserialize TDI report from host")
    }

    /// See: [`TdispVirtualDeviceInterface::tdisp_get_tdi_device_id`]
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

    /// See: [`TdispVirtualDeviceInterface::tdisp_unbind`]
    pub async fn tdisp_unbind(&mut self, reason: TdispGuestUnbindReason) -> anyhow::Result<()> {
        // Flip all unblocked MMIO ranges and DMA back to shared before we tell
        // the host to unbind the TDI. This is best-effort: a failure here is
        // logged but doesn't abort the unbind (this is not security critical to
        // perform).
        let validator = self.resource_validator.clone();
        let device_id = self.mutable_state.guest_device_id;
        let validated_bars_clone = self.mutable_state.validated_mmio_bars.clone();
        for (bar_id, mmio) in validated_bars_clone {
            // length == 0 is the "classified SHARED, never unblocked"
            // sentinel meaning there is nothing to block.
            if mmio.length_in_bytes == 0 {
                self.mutable_state.validated_mmio_bars.remove(&bar_id);
                continue;
            }
            if let Err(e) = validator.tdisp_block_mmio(
                Vtl::Vtl2,
                device_id,
                mmio.base_gpa,
                0,
                mmio.length_in_bytes,
                bar_id,
            ) {
                tracing::error!(
                    bar_id,
                    base_gpa = format_args!("{:#x}", mmio.base_gpa),
                    length_in_bytes = mmio.length_in_bytes,
                    error = &*e as &dyn std::error::Error,
                    "tdisp_unbind: failed to re-block MMIO range"
                );
            } else {
                // Successful re-block, remove the bar from the validated list.
                self.mutable_state.validated_mmio_bars.remove(&bar_id);
            }
        }

        if self.mutable_state.dma_unblocked {
            if let Err(e) = validator.tdisp_block_dma(Vtl::Vtl2, device_id) {
                tracing::error!(
                    device_id,
                    error = &*e as &dyn std::error::Error,
                    "tdisp_unbind: failed to re-block DMA"
                );
            } else {
                // Successful re-block, clear the DMA unblocked flag.
                self.mutable_state.dma_unblocked = false;
            }
        }

        // Clear every per-attest field. All of these will be fetched cleanly on
        // the next re-attest cycle.
        self.mutable_state.tdi_report = None;
        self.mutable_state.guest_device_id = 0;
        self.mutable_state.intercepted_bars.clear();

        // TEST SCAFFOLDING: the host unbind command is skipped under TDX
        // Connect because unbind is not functional there yet. Restore the call
        // below once it is. Note that skipping it leaves the guest's view of
        // the TDI state untouched (the host's response is what advances it), so
        // a subsequent re-attest will still see a non-Unlocked TDI.
        if matches!(self.isolation_type, IsolationType::Tdx) {
            tracing::warn!(
                device_id = self.vpci_device_id,
                ?reason,
                "tdisp_unbind: skipping host unbind command, unbind is not functional right now in TDX Connect"
            );
            return Ok(());
        }

        let res = self
            .send_tdisp_command(openhcl_tdisp::new_unbind_command(
                self.vpci_device_id,
                reason,
            ))
            .await?;

        match res.response::<TdispCommandResponseUnbind>() {
            Ok(_) => Ok(()),
            Err(err) => Err(anyhow::anyhow!("error response in tdisp_unbind: {err}")),
        }
    }

    /// Detects TDISP capabilities for the device. If the device supports TDISP
    /// and a guest protocol type that we support given the current VM's
    /// isolation level, then returns the interface info. Otherwise, returns an
    /// error representing why the device is not suitable for TDISP.
    ///
    /// Caches the first successful result on the state. Device
    /// capabilities are static across the VM's lifetime, so subsequent
    /// calls return the cached copy without issuing another host
    /// command. A failure is not cached, so the next call will retry.
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
    /// See: [`TdispVpciAttestationInterface::tdisp_attest_device`]
    pub async fn query_capabilities(&mut self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        anyhow::bail!("TDISP feature not enabled during compile time")
    }

    /// See: [`TdispVpciAttestationInterface::tdisp_attest_device`]
    pub async fn attest(&mut self, interface_info: TdispDeviceInterfaceInfo) -> anyhow::Result<()> {
        tracing::info!(
            ?interface_info,
            "tdisp_attest_device: beginning attestation flow"
        );

        // If there are any existing attestation artifacts, we need to clear
        // them before starting a new attestation.
        if self.tdi_state() != TdispTdiState::Unlocked
            || self.mutable_state.dma_unblocked
            || !self.mutable_state.validated_mmio_bars.is_empty()
        {
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
        if self.tdi_state() != TdispTdiState::Unlocked
            || self.mutable_state.dma_unblocked
            || !self.mutable_state.validated_mmio_bars.is_empty()
        {
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

        self.resource_validator
            .on_pre_start(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: pre-start validation failed")?;

        self.tdisp_start_device()
            .await
            .context("tdisp_attest_device: failed to start device")?;

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
            .update_guest_device_id(guest_device_id_u16);

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
        // performed. Platform specific validation methods will be called on
        // command register write to unblock resources.
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
    /// `tdisp_on_mmio_reconfigured` must NOT call `tdisp_unblock_mmio` on such
    /// a BAR because it is not RAM.
    pub fn mark_bar_intercepted(&mut self, bar_id: u16) {
        if self.mutable_state.intercepted_bars.insert(bar_id) {
            tracing::info!(
                bar_id,
                "marking BAR as intercepted; TDISP MMIO unblock will be skipped for this BAR"
            );
        }
    }

    /// Returns true if the given BAR has been marked intercepted
    /// via [`Self::mark_bar_intercepted`].
    pub fn is_bar_intercepted(&self, bar_id: u16) -> bool {
        self.mutable_state.intercepted_bars.contains(&bar_id)
    }

    /// Classify a single BAR's isolation, based solely on the cached TDI
    /// interface report and `intercepted_bars`.
    ///
    /// This is the single source of truth used by both
    /// [`Self::isolation_snapshot`] and [`Self::tdisp_on_mmio_reconfigured`]: a
    /// BAR whose classification here is `PRIVATE` is exactly one that
    /// `tdisp_on_mmio_reconfigured` will call `tdisp_unblock_mmio` for, while
    /// `SHARED` is skipped. `INVALID` means the BAR has no entry in the report.
    fn classify_bar(&self, bar_id: u16) -> ResourceIsolation {
        // Host-intercepted BARs (MSI-X table / PBA) have no host-RAM
        // backing and can never be flipped private, so always SHARED,
        // independent of what the report says.
        if self.mutable_state.intercepted_bars.contains(&bar_id) {
            return ResourceIsolation::SHARED;
        }

        // No cached report yet (attestation hasn't run) → we don't know
        // if this BAR is claimed at all, so INVALID rather than SHARED.
        let Some(report) = self.mutable_state.tdi_report.as_ref() else {
            return ResourceIsolation::INVALID;
        };

        // `range_id` == PCI BAR index for the guest protocols we
        // support. A missing entry is an unused slot or the upper half
        // of a 64-bit BAR (not reported independently).
        let Some(range) = report
            .mmio_interface_info
            .iter()
            .find(|r| r.range_id == bar_id)
        else {
            return ResourceIsolation::INVALID;
        };

        // `is_non_tee_mem` ranges have no protected backing and must
        // never be passed to `tdisp_unblock_mmio`. Report SHARED and
        // skip. Everything else is TEE memory the TDI owns → PRIVATE.
        if range.flags.is_non_tee_mem() {
            ResourceIsolation::SHARED
        } else {
            ResourceIsolation::PRIVATE
        }
    }

    /// Classify BAR and DMA isolation for this device at this instant,
    /// suitable for populating a `VpciIsolatedResourcesReply` on the
    /// guest-facing side.
    ///
    /// Returns [`IsolationSnapshot::NotReady`] iff no TDI interface
    /// report has been cached yet (attestation has not run). Once a
    /// report is cached, always returns `Ready`, including after a
    /// `tdisp_unbind_preserve_report` has returned the TDI to
    /// `Unlocked`. Classification mirrors the logic that
    /// [`Self::tdisp_on_mmio_reconfigured`] applies when the guest
    /// enables MMIO: a BAR is `PRIVATE` exactly when
    /// `tdisp_unblock_mmio` would be called for it, `SHARED` when it
    /// would be skipped, and `INVALID` when the cached TDI report has
    /// no entry for it.
    pub fn isolation_snapshot(&self) -> IsolationSnapshot {
        if self.mutable_state.tdi_report.is_none() {
            return IsolationSnapshot::NotReady;
        }

        let mut bars = [ResourceIsolation::INVALID; 6];
        for bar_id in 0..6u16 {
            bars[bar_id as usize] = self.classify_bar(bar_id);
        }

        let dma: ResourceIsolation = {
            // If any BAR is classified as PRIVATE, the the device should also have PRIVATE DMA.
            if bars.contains(&ResourceIsolation::PRIVATE) {
                // TDISP devices with private MMIO always have private DMA, even
                // if at this moment the device's DMA isn't unblocked.
                ResourceIsolation::PRIVATE
            } else {
                ResourceIsolation::SHARED
            }
        };

        IsolationSnapshot::Ready { bars, dma }
    }

    /// Called when a BAR MMIO range is reconfigured by the guest. If a resource
    /// validator is present, unblocks the MMIO range for the device.
    ///
    /// Consults the TDI interface report saved during attestation to decide
    /// whether the MMIO range actually requires validation. Ranges whose
    /// `is_non_tee_mem` flag is set are not protected memory and must NOT be
    /// passed to `tdisp_unblock_mmio`. Only ranges with `is_non_tee_mem` clear
    /// are validated.
    ///
    /// BARs that the paravisor has marked as intercepted (see
    /// [`Self::mark_bar_intercepted`]) are skipped unconditionally because
    /// these pages have no host-side RAM backing.
    ///
    /// # Arguments
    ///
    /// * `bar_id` - The BAR index being configured. Matched against the
    ///   `range_id` of the MMIO ranges reported in the TDI interface report.
    /// * `base_address` - The base guest physical address of the MMIO range.
    /// * `length` - The length in bytes of the MMIO range.
    pub fn tdisp_on_mmio_reconfigured(
        &mut self,
        bar_id: u16,
        base_address: u64,
        length: u32,
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
            ResourceIsolation::SHARED => {
                tracing::info!(
                    bar_id,
                    base_address,
                    length,
                    "skipping MMIO unblock for BAR classified SHARED \
                     (intercepted or non-TEE memory)"
                );
                // Record with a zero-length entry so we don't repeatedly
                // fall through here on subsequent reconfigurations. The
                // unbind path uses length == 0 as a sentinel for "no
                // block call needed."
                self.mutable_state.validated_mmio_bars.insert(
                    bar_id,
                    ValidatedMmio {
                        base_gpa: base_address,
                        length_in_bytes: 0,
                    },
                );
                return Ok(());
            }
            ResourceIsolation::INVALID => {
                anyhow::bail!(
                    "tdisp_on_mmio_reconfigured: BAR {bar_id} has no entry in \
                     the TDI interface report (or report not available); \
                     device has not been attested"
                );
            }
            ResourceIsolation::PRIVATE => {}
            other => {
                anyhow::bail!(
                    "tdisp_on_mmio_reconfigured: unexpected BAR {bar_id} \
                     classification {:?}",
                    other
                );
            }
        }

        let device_id = self.mutable_state.guest_device_id;

        self.resource_validator
            .tdisp_unblock_mmio(self.target_vtl, device_id, base_address, 0, length, bar_id)
            .context("tdisp_on_mmio_reconfigured: failed to unblock MMIO")?;

        self.mutable_state.validated_mmio_bars.insert(
            bar_id,
            ValidatedMmio {
                base_gpa: base_address,
                length_in_bytes: length,
            },
        );

        // After the first successful MMIO unblock following attestation,
        // unblock DMA as well so the device can issue DMA traffic to the
        // guest. Guard with `dma_unblocked` so it only fires once per
        // bind/attest cycle (cleared on unbind).
        if !self.mutable_state.dma_unblocked {
            self.resource_validator
                .tdisp_unblock_dma(self.target_vtl, device_id)
                .context("tdisp_on_mmio_reconfigured: failed to unblock DMA")?;
            self.mutable_state.dma_unblocked = true;
            tracing::info!(device_id, "tdisp_on_mmio_reconfigured: DMA unblocked");
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
}

/// Higher level interface for TDISP operations on a VPCI device.
#[allow(async_fn_in_trait)]
pub trait TdispVpciAttestationInterface: Sync + Send {
    /// Attests the device using the TDISP flow. This includes binding the
    /// device, starting it, and any other validation steps on reports that are
    /// necessary for the device to be considered attested.
    ///
    /// This is a higher level function that wraps the lower level TDISP
    /// operations such as bind and start, and is designed to be called by
    /// higher level code to perform the attestation process atomically.
    ///
    /// The resulting device state will be Run with resoures unblocked if
    /// attestation is successful (Ok() is returned), or Unlocked if attestation
    /// fails (Err() is returned).
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

    /// Called when a BAR MMIO range is reconfigured by the guest. If a resource
    /// validator is present, unblocks the MMIO range for the device.
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
        length: u32,
    ) -> anyhow::Result<()>;

    /// Mark a BAR as paravisor-intercepted so that TDISP will skip calling
    /// `tdisp_unblock_mmio` on it during MMIO reconfiguration. Use this for
    /// BARs whose memory is registered as a paravisor MMIO intercept region
    /// (e.g. the MSI-X table / PBA BAR) and therefore has no host-side RAM
    /// backing that could be flipped to private.
    async fn tdisp_mark_bar_intercepted(&self, bar_id: u16);

    /// Return a classification of BAR and DMA isolation for this device.
    /// Callers on the guest-facing VPCI channel use this to synthesize the
    /// `VpciIsolatedResourcesReply` for `VPCI_QUERY_ISOLATED_RESOURCES`.
    async fn tdisp_isolation_snapshot(&self) -> IsolationSnapshot;
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
        length: u32,
    ) -> anyhow::Result<()> {
        let mut guard = self.tdisp.0.lock().await;
        guard.tdisp_on_mmio_reconfigured(bar_id, base_address, length)
    }

    async fn tdisp_mark_bar_intercepted(&self, bar_id: u16) {
        let mut guard = self.tdisp.0.lock().await;
        guard.mark_bar_intercepted(bar_id);
    }

    async fn tdisp_isolation_snapshot(&self) -> IsolationSnapshot {
        let guard = self.tdisp.0.lock().await;
        guard.isolation_snapshot()
    }
}

impl VpciDevice {
    /// Async equivalent of [`Self::tdisp_try_isolation_snapshot`]: awaits
    /// the per-device TDISP mutex and returns the current isolation
    /// snapshot. Use from async contexts where blocking on the mutex is
    /// acceptable (e.g. the guest-facing VPCI dispatch on its async path).
    pub async fn tdisp_isolation_snapshot(&self) -> IsolationSnapshot {
        let guard = self.tdisp.0.lock().await;
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
            IsolationSnapshot::NotReady
        ));

        let mut state = new_state();
        state.mutable_state.tdi_state = TdispTdiState::Run;
        assert!(matches!(
            state.isolation_snapshot(),
            IsolationSnapshot::NotReady
        ));
    }

    #[test]
    fn isolation_snapshot_ready_with_empty_report() {
        // Cached (empty) report → Ready; every BAR INVALID, DMA SHARED.
        // No TDI-state requirement.
        let mut state = new_state();
        state.mutable_state.tdi_report = Some(make_report(vec![]));
        let IsolationSnapshot::Ready { bars, dma } = state.isolation_snapshot() else {
            panic!("expected Ready");
        };
        assert_eq!(bars, [ResourceIsolation::INVALID; 6]);
        assert_eq!(dma, ResourceIsolation::SHARED);
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
        let IsolationSnapshot::Ready { bars, dma } = state.isolation_snapshot() else {
            panic!("expected Ready");
        };
        assert_eq!(
            bars,
            [
                ResourceIsolation::PRIVATE,
                ResourceIsolation::INVALID,
                ResourceIsolation::SHARED,
                ResourceIsolation::INVALID,
                ResourceIsolation::SHARED,
                ResourceIsolation::INVALID,
            ]
        );
        assert_eq!(dma, ResourceIsolation::PRIVATE);
    }

    #[test]
    fn isolation_snapshot_dma_private_with_any_private_mmio() {
        let mut state = new_state();
        state.mutable_state.tdi_report = Some(make_report(vec![tee_range(0)]));
        let IsolationSnapshot::Ready { bars, dma } = state.isolation_snapshot() else {
            panic!("expected Ready");
        };
        assert_eq!(
            bars,
            [
                ResourceIsolation::PRIVATE,
                ResourceIsolation::INVALID,
                ResourceIsolation::INVALID,
                ResourceIsolation::INVALID,
                ResourceIsolation::INVALID,
                ResourceIsolation::INVALID,
            ]
        );
        assert_eq!(dma, ResourceIsolation::PRIVATE);
    }
}
