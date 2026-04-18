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
use vpci_protocol::SlotNumber;

use super::VpciDevice;
use super::WorkerRequest;
use openhcl_tdisp::TdispResourceValidationInterface;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Inspect)]
struct VpciClientTdispMutableState {
    tdi_state: TdispTdiState,
    guest_device_id: u16,
    /// Set of BAR IDs that have been successfully validated via `tdisp_unblock_mmio`.
    /// Cleared on unbind so that ranges are re-validated after re-attestation.
    #[inspect(iter_by_index)]
    validated_mmio_bars: HashSet<u16>,
    /// Whether DMA has been unblocked via `tdisp_unblock_dma`. Cleared on
    /// unbind so that DMA is re-unblocked after re-attestation.
    dma_unblocked: bool,
    /// The most recently obtained TDI interface report, populated during attestation.
    /// Cleared on unbind so that it is re-fetched after re-attestation.
    #[inspect(debug)]
    tdi_report: Option<TdiReportStruct>,
    /// Set of BAR IDs whose MMIO pages are intercepted (e.g. a BAR that hosts
    /// the MSI-X table / PBA emulated by the host). These pages are not backed
    /// by convertible guest RAM on the host.
    #[inspect(iter_by_index)]
    intercepted_bars: HashSet<u16>,
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
    #[inspect(skip)]
    resource_validator: Option<Arc<dyn TdispResourceValidationInterface>>,
}

/// Manages the TDISP protocol for a TDISP-capable VPCI device.
impl VpciClientTdispState {
    pub(super) fn new(
        worker_req: mesh::Sender<WorkerRequest>,
        device_id: u64,
        resource_validator: Option<Arc<dyn TdispResourceValidationInterface>>,
        isolation_type: IsolationType,
        vtom: u64,
        target_vtl: Vtl,
    ) -> Self {
        Self {
            worker_req,
            vpci_device_id: device_id,
            mutable_state: VpciClientTdispMutableState {
                tdi_state: TdispTdiState::Uninitialized,
                guest_device_id: 0,
                validated_mmio_bars: HashSet::new(),
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
            _ => {
                let err_msg = format!(
                    "send_tdisp_command {:?} failed because host responded with an error: {:?}",
                    payload.type_name(),
                    res.result
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
        let res = self
            .send_tdisp_command(openhcl_tdisp::new_unbind_command(
                self.vpci_device_id,
                reason,
            ))
            .await?;

        match res.response::<TdispCommandResponseUnbind>() {
            Ok(_) => {
                self.mutable_state.validated_mmio_bars.clear();
                self.mutable_state.dma_unblocked = false;
                self.mutable_state.tdi_report = None;
                Ok(())
            }
            Err(err) => Err(anyhow::anyhow!("error response in tdisp_unbind: {err}")),
        }
    }

    /// Detects TDISP capabilities for the device. If the device supports TDISP
    /// and a guest protocol type that we support given the current VM's
    /// isolation level, then returns the interface info. Otherwise, returns an
    /// error representing why the device is not suitable for TDISP.
    #[cfg(feature = "dev_snp_ohcl_tio_support")]
    pub async fn query_capabilities(&mut self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        tracing::info!(
            ?self.isolation_type,
            "querying TDISP capabilities for device given VM isolation type"
        );

        let target_protocol = match self.isolation_type {
            IsolationType::Snp => TdispGuestProtocolType::AmdSevTioV1,
            IsolationType::Tdx => {
                tracing::warn!(
                    "query_capabilities: VM is running with TDX isolation (NOT SUPPORTED)"
                );
                anyhow::bail!("TDX isolation is not currently supported for TDISP")
            }
            IsolationType::Vbs => {
                tracing::warn!(
                    "query_capabilities: VM is running with VBS isolation (NOT SUPPORTED)"
                );
                anyhow::bail!("VBS isolation is not currently supported for TDISP")
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

        // TDISP TODO: Support TDX
        let expected_guest_protocol = TdispGuestProtocolType::AmdSevTioV1;
        if device_interface_info.guest_protocol_type == expected_guest_protocol as i32 {
            tracing::info!(
                ?device_interface_info.guest_protocol_type,
                "tdisp_query_capabilities: TDISP is supported",
            );

            Ok(device_interface_info)
        } else {
            tracing::info!(
                ?device_interface_info.guest_protocol_type,
                ?expected_guest_protocol,
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

        self.tdisp_bind_interface()
            .await
            .context("tdisp_attest_device: failed to bind device interface")?;

        self.tdisp_start_device()
            .await
            .context("tdisp_attest_device: failed to start device")?;

        // Request the guest device ID to use in firmware calls to unblock resources
        let guest_device_id = self
            .tdisp_get_tdi_device_id()
            .await
            .context("tdisp_attest_device: failed to get TDI device ID after starting device")?;

        // Platforms require a u16 device ID even though the report returns a
        // u64. Ensure the returned device ID fits within that constraint before
        // proceeding.
        let guest_device_id_u16 = u16::try_from(guest_device_id)
            .context("tdisp_attest_device: guest device ID must fit within u16")?;

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
        // on the host and therefore cannot be made private.
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

    /// Mark a BAR as being intercepted and virtualized by the paravisor
    /// (e.g. a BAR whose memory is registered as [`BarMemoryKind::Intercept`]
    /// — the classic case being the MSI-X table / PBA BAR, which is handled
    /// entirely inside the paravisor's VPCI layer and has no host-side
    /// backing page).
    ///
    /// `tdisp_on_mmio_reconfigured` must NOT call `tdisp_unblock_mmio` on
    /// such a BAR: the underlying `HvCallModifySparseGpaPageHostVisibility`
    /// hypercall on a non-convertible GPA terminates the partition. This
    /// provides the paravisor a way to tell TDISP "I handle this BAR, leave
    /// it alone" before any guest configuration occurs.
    ///
    /// Intentionally intercept-vs-not is a static property of the device's
    /// emulator, so this state is *not* cleared on unbind.
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

    /// Called when a BAR MMIO range is reconfigured by the guest. If a resource
    /// validator is present, unblocks the MMIO range for the device.
    ///
    /// Consults the TDI interface report saved during attestation to decide
    /// whether the MMIO range actually requires validation. Ranges whose
    /// `is_non_tee_mem` flag is set are not protected memory and must NOT be
    /// passed to `tdisp_unblock_mmio`; only ranges with `is_non_tee_mem` clear
    /// are validated.
    ///
    /// BARs that the paravisor has marked as intercepted (see
    /// [`Self::mark_bar_intercepted`]) are skipped unconditionally — these
    /// pages have no host-side RAM backing and must never be flipped to
    /// private.
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
        if let Some(validator) = &self.resource_validator {
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

            // This BAR is marked as intercepted by an emulator. There is no
            // convertible guest-RAM page backing this GPA on the host, so
            // calling tdisp_unblock_mmio cannot mark anything as private. Skip
            // entirely.
            if self.mutable_state.intercepted_bars.contains(&bar_id) {
                tracing::info!(
                    bar_id,
                    base_address,
                    length,
                    "skipping MMIO unblock for BAR because it is an intercepted region"
                );
                self.mutable_state.validated_mmio_bars.insert(bar_id);
                return Ok(());
            }

            if self.mutable_state.validated_mmio_bars.contains(&bar_id) {
                tracing::debug!(
                    bar_id,
                    "skipping MMIO unblock for BAR that has already been validated"
                );
                return Ok(());
            }

            // Look up the MMIO range in the TDI interface report by range_id
            // (which matches the BAR index for the guest protocols we
            // currently support). Only TEE memory (is_non_tee_mem == false)
            // should be passed to tdisp_unblock_mmio; non-TEE ranges are
            // unprotected and must not be validated.
            let report = self.mutable_state.tdi_report.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "tdisp_on_mmio_reconfigured: TDI interface report not available; device has not been attested"
                )
            })?;

            let mmio_range = report
                .mmio_interface_info
                .iter()
                .find(|r| r.range_id == bar_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "tdisp_on_mmio_reconfigured: BAR {bar_id} not present in TDI interface report"
                    )
                })?;

            if mmio_range.flags.is_non_tee_mem() {
                tracing::info!(
                    bar_id,
                    base_address,
                    length,
                    "skipping MMIO unblock for BAR because the TDI report marks it as non-TEE memory"
                );
                // Record the BAR as handled so we don't repeatedly re-check
                // the report on subsequent reconfiguration callbacks.
                self.mutable_state.validated_mmio_bars.insert(bar_id);
                return Ok(());
            }

            let device_id = self.mutable_state.guest_device_id;

            validator.tdisp_unblock_mmio(
                self.target_vtl,
                device_id,
                base_address,
                0,
                length,
                bar_id,
            )?;
            self.mutable_state.validated_mmio_bars.insert(bar_id);

            // After the first successful MMIO unblock following attestation,
            // unblock DMA as well so the device can issue DMA traffic to the
            // guest. Guard with `dma_unblocked` so it only fires once per
            // bind/attest cycle (cleared on unbind).
            if !self.mutable_state.dma_unblocked {
                validator
                    .tdisp_unblock_dma(self.target_vtl, device_id)
                    .context("tdisp_on_mmio_reconfigured: failed to unblock DMA")?;
                self.mutable_state.dma_unblocked = true;
                tracing::info!(device_id, "tdisp_on_mmio_reconfigured: DMA unblocked");
            }

            Ok(())
        } else {
            Ok(())
        }
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
    /// higher level code that just wants to attest the device without needing
    /// to orchestrate the individual steps of the TDISP flow.
    ///
    /// Resources will remain locked until the guest calls platform-specific validation
    /// methods to unblock them (e.g. SEV platform firmware calls). This method does not perform those
    /// validation steps.
    ///
    /// The resulting device state will be Run if attestation is successful (Ok() is returned), or
    /// Unlocked if attestation fails (Err() is returned).
    async fn tdisp_attest_device(
        &self,
        interface_info: TdispDeviceInterfaceInfo,
    ) -> anyhow::Result<()>;

    /// Detects TDISP capabilities for the device. If the device supports TDISP
    /// and a guest protocol type that we support given the current VM's
    /// isolation level, then returns the interface info. Otherwise, returns an
    /// error representing why the device is not suitable for TDISP.
    async fn tdisp_query_capabilities(&self) -> anyhow::Result<TdispDeviceInterfaceInfo>;

    /// Get the TDI state of the device. This is used for testing and validation purposes, and is not part of the standard TDISP flow.
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
}
