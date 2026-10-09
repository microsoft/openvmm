// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The guest side of the TDISP protocol, shared by every virtual bus.
//!
//! This code is bus-agnostic and handles higher-level TDISP client logic.
//! Busses implement [TdispCommandTransport] and provide the underlying
//! communication channel for TDISP commands.

use crate::GuestToHostCommand;
use crate::GuestToHostCommandExt;
use crate::GuestToHostResponse;
use crate::GuestToHostResponseExt;
use crate::TdispCommandResponseBind;
use crate::TdispCommandResponseGetDeviceInterfaceInfo;
use crate::TdispCommandResponseGetTdiReport;
use crate::TdispCommandResponseModifyMmioRange;
use crate::TdispCommandResponseStartTdi;
use crate::TdispDeviceInterfaceInfo;
use crate::TdispGuestOperationErrorCode;
use crate::TdispGuestProtocolType;
use crate::TdispGuestUnbindReason;
use crate::TdispReportType;
use crate::TdispResourceValidationInterface;
use anyhow::Context;
use hvdef::Vtl;
use inspect::Inspect;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tdisp::TdispIsolationReport;
use tdisp::TdispResourceIsolation;
use tdisp::TdispTdiState;
use tdisp::devicereport::TdiReportStruct;
use virt::IsolationType;

/// Carries TDISP guest-to-host commands over a particular bus.
pub trait TdispCommandTransport: Send + Sync + Inspect {
    /// The identifier the bus uses to address this device, carried in the
    /// `device_id` field of every guest-to-host command. Independent of the
    /// TDI device id used by platform specific firmware calls.
    fn bus_device_id(&self) -> u64;

    /// Send a command to the host and wait for its response.
    ///
    /// An error means the command did not complete a round trip. A command the
    /// host answered with a TDISP error is still `Ok` here, and the caller
    /// reads the outcome from the response.
    ///
    /// * `command` - The command to send.
    fn send_command<'a>(
        &'a self,
        command: GuestToHostCommand,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<GuestToHostResponse>> + Send + Sync + 'a>>;
}

/// A TDISP-capable device, driven through a bus transport.
///
/// Operations are serialized against each other, so this can be shared and
/// called concurrently.
pub struct TdispClient(futures::lock::Mutex<TdispClientState>);

impl Inspect for TdispClient {
    fn inspect(&self, req: inspect::Request<'_>) {
        match self.0.try_lock() {
            Some(guard) => guard.inspect(req),
            None => req.value("locked"),
        }
    }
}

impl TdispClient {
    /// * `transport` - Carries commands to the host over the bus this device
    ///   is on.
    /// * `resource_validator` - Platform hooks that gate attestation and
    ///   unblock device resources.
    /// * `isolation_type` - The isolation type of the partition the device is
    ///   assigned to, which decides the guest protocol to negotiate.
    /// * `target_vtl` - The VTL the device is assigned to.
    /// * `bar_masks` - Which BARs the device implements.
    pub fn new(
        transport: Box<dyn TdispCommandTransport>,
        resource_validator: Arc<dyn TdispResourceValidationInterface>,
        isolation_type: IsolationType,
        target_vtl: Vtl,
        bar_masks: [bool; 6],
    ) -> Self {
        Self(futures::lock::Mutex::new(TdispClientState::new(
            transport,
            resource_validator,
            isolation_type,
            target_vtl,
            bar_masks,
        )))
    }

    /// The TDI state the host reported for the most recent operation.
    pub async fn tdi_state(&self) -> TdispTdiState {
        self.0.lock().await.tdi_state()
    }

    /// Negotiate a guest protocol with the host and return the device's
    /// interface info.
    ///
    /// * `target_protocol` - The guest protocol to negotiate.
    pub async fn get_device_interface_info(
        &self,
        target_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        self.0
            .lock()
            .await
            .get_device_interface_info(target_protocol)
            .await
    }

    /// Detect TDISP capabilities for the device. Returns the interface info if
    /// the device supports TDISP and a guest protocol that matches the
    /// partition's isolation type, and otherwise an error saying why the device
    /// is not suitable for TDISP.
    pub async fn query_capabilities(&self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        self.0.lock().await.query_capabilities().await
    }

    /// Run the full attestation flow, leaving the TDI in Run with its interface
    /// report cached. Any prior attestation is torn down first, so this is safe
    /// to call from any TDI state.
    ///
    /// Device resources are not accessible on return. They are unblocked later,
    /// when the guest enables MMIO.
    ///
    /// * `interface_info` - The negotiated capabilities for this device.
    pub async fn attest(&self, interface_info: TdispDeviceInterfaceInfo) -> anyhow::Result<()> {
        self.0.lock().await.attest(interface_info).await
    }

    /// Fetch and decode the device's TDI interface report.
    pub async fn get_tdi_report(&self) -> anyhow::Result<TdiReportStruct> {
        self.0.lock().await.get_tdi_report().await
    }

    /// Unbind the device, returning the TDI to Unlocked and dropping all
    /// per-attest state so the next attestation starts clean.
    ///
    /// * `reason` - Reported to the host to explain why the TDI is unbinding.
    pub async fn unbind(&self, reason: TdispGuestUnbindReason) {
        self.0.lock().await.unbind(reason).await
    }

    /// Called when the guest reconfigures a BAR's MMIO range, to make the range
    /// accessible if it is private memory.
    ///
    /// * `bar_id` - The BAR index being configured.
    /// * `base_address` - The base guest physical address of the MMIO range.
    /// * `length` - The length in bytes of the MMIO range.
    pub async fn on_mmio_reconfigured(
        &self,
        bar_id: u16,
        base_address: u64,
        length: u64,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .await
            .on_mmio_reconfigured(bar_id, base_address, length)
            .await
    }

    /// Mark a BAR as paravisor-intercepted so that it is always considered
    /// shared. Use this for BARs whose memory is registered as a MMIO intercept
    /// region, such as the MSI-X table and PBA BAR, which have no host-side
    /// RAM.
    ///
    /// * `bar_id` - The BAR index to mark.
    pub async fn mark_bar_intercepted(&self, bar_id: u16) {
        self.0.lock().await.mark_bar_intercepted(bar_id)
    }

    /// Classify BAR and DMA isolation for this device as it stands right now.
    ///
    /// Returns `NotReady` if no TDI interface report is cached.
    pub async fn isolation_snapshot(&self) -> TdispIsolationReport {
        self.0.lock().await.isolation_snapshot()
    }

    /// Classify BAR and DMA isolation, attesting the device first if it has not
    /// been attested yet.
    ///
    /// Returns `NotTdispCapable` if the device turns out not to support TDISP,
    /// and `Error` if attestation fails.
    pub async fn isolation_snapshot_attested(&self) -> TdispIsolationReport {
        let mut state = self.0.lock().await;

        if state.tdi_state() == TdispTdiState::Unlocked {
            let info = match state.query_capabilities().await {
                Ok(info) => info,
                Err(err) => {
                    tracing::error!(
                        error = &*err as &dyn std::error::Error,
                        "isolation_snapshot_attested: query_capabilities failed, \
                         TDISP is unsupported or the host errored out trying to start TDISP"
                    );
                    return TdispIsolationReport::NotTdispCapable;
                }
            };

            if let Err(err) = state.attest(info).await {
                tracing::error!(
                    error = &*err as &dyn std::error::Error,
                    "isolation_snapshot_attested: attest from Unlocked failed",
                );
                return TdispIsolationReport::Error;
            }
        }

        state.isolation_snapshot()
    }
}

#[derive(Inspect)]
struct TdispClientMutableState {
    tdi_state: TdispTdiState,
    #[inspect(debug)]
    guest_device_id: TdispDeviceId,
    /// Map of BAR ID to the range the guest configured, how it was classified
    /// by the TDI report, and its current protection state. Cleared on unbind.
    #[inspect(iter_by_key)]
    validated_mmio_bars: std::collections::HashMap<u16, ValidatedMmio>,
    /// Whether DMA has been unblocked via `tdisp_unblock_dma`. Cleared on
    /// unbind so that DMA is re-unblocked after re-attestation.
    dma_unblocked: bool,
    /// The most recently obtained TDI interface report populated during attestation.
    /// Cleared on unbind so that it is re-fetched after re-attestation.
    #[inspect(debug)]
    tdi_report: Option<TdiReportStruct>,
    /// Set of BAR IDs whose MMIO pages are intercepted (e.g. a BAR that hosts
    /// the MSI-X table / PBA emulated by the host). These pages are not backed
    /// by guest RAM on the host and are always marked SHARED.
    #[inspect(iter_by_index)]
    intercepted_bars: HashSet<u16>,
}

/// Identifies the TDI to the host (distinct from the bus's device id).
///
/// A TDI only has an id once attestation has fetched one from the host.
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

    /// How the range was classified from the report.
    #[inspect(debug)]
    isolation: TdispResourceIsolation,
}

impl TdispClientMutableState {
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

struct SetupDeviceFailure {
    reason: TdispGuestUnbindReason,
    message: String,
}

/// TDISP state for a single device, guarded by [`TdispClient`].
#[derive(Inspect)]
struct TdispClientState {
    /// The bus transport used to communicate with the host.
    transport: Box<dyn TdispCommandTransport>,
    /// The isolation type the VM is running in.
    isolation_type: IsolationType,
    /// Target VTL the TDI will be assigned to.
    #[inspect(debug)]
    target_vtl: Vtl,
    /// State that is mutable and can change over the lifetime of the device and
    /// cleared on Unbind.
    mutable_state: TdispClientMutableState,
    /// Which BAR indices the device actually implements. Fixed for the life of
    /// the device.
    #[inspect(iter_by_index)]
    bar_masks: [bool; 6],
    /// Platform hooks used to gate attestation and unblock device resources.
    #[inspect(skip)]
    resource_validator: Arc<dyn TdispResourceValidationInterface>,
}

impl TdispClientState {
    fn new(
        transport: Box<dyn TdispCommandTransport>,
        resource_validator: Arc<dyn TdispResourceValidationInterface>,
        isolation_type: IsolationType,
        target_vtl: Vtl,
        bar_masks: [bool; 6],
    ) -> Self {
        Self {
            transport,
            mutable_state: TdispClientMutableState {
                tdi_state: TdispTdiState::Unlocked,
                guest_device_id: TdispDeviceId::Invalid,
                validated_mmio_bars: std::collections::HashMap::new(),
                dma_unblocked: false,
                tdi_report: None,
                intercepted_bars: HashSet::new(),
            },
            isolation_type,
            target_vtl,
            bar_masks,
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
    /// In TDISP, the host controls the TDI lifecycle states and transitions.
    /// The host's reported state and the trusted firmware's state are two
    /// independent sources.. Either source diverging from it means the
    /// paravisor's view of the device is wrong, which is not a condition it can
    /// recover from or safely continue past. A platform that cannot report its
    /// own state (test environments) leaves only the host's answer to check.
    ///
    /// * `expected` - The state the TDI must be in.
    /// * `device_id` - Identifies the TDI device (not the bus's device ID).
    ///   Only valid if the platform supports reporting its own TDI state.
    fn require_tdi_state(&self, expected: TdispTdiState, device_id: TdispDeviceId) {
        let cached = self.tdi_state();

        // Read the firmware first even when the host's answer is already wrong,
        // so the panic can report both values.
        let firmware = match device_id.id() {
            Some(device_id) => self
                .resource_validator
                .get_tsm_tdi_state(self.target_vtl, device_id)
                .unwrap_or_else(|e| {
                    panic!("require_tdi_state: failed to read the TDI state from the firmware: {e}")
                }),
            None => {
                // Require that any state beyond Unlocked is not allowed when the device ID is not assigned.
                if cached != TdispTdiState::Unlocked || expected != TdispTdiState::Unlocked {
                    panic!(
                        "require_tdi_state: device ID wasn't assigned when calling require_tdi_state, \
                         but the host reports {cached}"
                    );
                }

                None
            }
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
    }

    /// Send a command to the host over the bus transport and record the TDI
    /// state the host reports back.
    ///
    /// * `payload` - The command to send.
    async fn send_command(
        &mut self,
        payload: GuestToHostCommand,
    ) -> anyhow::Result<GuestToHostResponse> {
        // Capture the name before the command moves, for the error path.
        let command_name = payload.type_name().map(|name| name.to_string());

        let res = self.transport.send_command(payload).await?;

        // Record state transitions based on the TDI state returned by the host in the response, if available.
        match res.tdi_state_after_enum() {
            Some(state) => self.mutable_state.update_tdi_state(state),
            None => std::panic!("tdisp: host returned a completely unknown TDI state in response"),
        }

        match res.error_code() {
            Some(TdispGuestOperationErrorCode::Success) => Ok(res),
            other => {
                let err_name = match other {
                    Some(code) => format!("{code:?}"),
                    None => format!("Unknown({})", res.result),
                };
                let err_msg = format!(
                    "send_command {:?} failed because host responded with an error: {}",
                    command_name, err_name,
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
    async fn get_device_interface_info(
        &mut self,
        target_protocol: TdispGuestProtocolType,
    ) -> anyhow::Result<TdispDeviceInterfaceInfo> {
        let res = self
            .send_command(crate::new_get_device_interface_info_command(
                self.transport.bus_device_id(),
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
    async fn bind_interface(&mut self) -> anyhow::Result<()> {
        let state_before = self.tdi_state();
        let res = self
            .send_command(crate::new_bind_command(self.transport.bus_device_id()))
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
    async fn start_device(&mut self) -> anyhow::Result<()> {
        let state_before = self.tdi_state();
        let res = self
            .send_command(crate::new_start_tdi_command(self.transport.bus_device_id()))
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
    async fn get_device_report(
        &mut self,
        report_type: &TdispReportType,
    ) -> anyhow::Result<Vec<u8>> {
        let res = self
            .send_command(crate::new_get_tdi_report_command(
                self.transport.bus_device_id(),
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
    async fn get_tdi_report(&mut self) -> anyhow::Result<TdiReportStruct> {
        let buffer = self
            .get_device_report(&TdispReportType::InterfaceReport)
            .await
            .context("failed to get TDI report")?;

        // Log the raw bytes before parsing them, so a report that fails to
        // deserialize can still be decoded by hand from the trace.
        tracing::info!(
            device_id = self.transport.bus_device_id(),
            len = buffer.len(),
            raw = format_args!("{buffer:02x?}"),
            "tdisp_get_tdi_report: raw TDI interface report from the host"
        );

        let report = tdisp::devicereport::deserialize_tdi_report(&buffer)
            .context("failed to deserialize TDI report from host")?;

        tracing::info!(
            device_id = self.transport.bus_device_id(),
            ?report,
            "tdisp_get_tdi_report: decoded TDI interface report"
        );

        for range in &report.mmio_interface_info {
            tracing::debug!(
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

    /// Tell the host to block an MMIO range, reversing a previous unblock.
    /// This only notifies the host; the platform-side block is separate.
    ///
    /// * `range_id` - Identifies which MMIO range to block (the PCI BAR index).
    /// * `gpa_base` - The guest physical base address of the range.
    /// * `range_len_bytes` - The length of the range, in bytes.
    async fn host_block_mmio_range(
        &mut self,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        self.send_modify_mmio_range(
            crate::new_block_mmio_range_command(
                self.transport.bus_device_id(),
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
    async fn host_unblock_mmio_range(
        &mut self,
        range_id: u16,
        gpa_base: u64,
        range_len_bytes: u64,
    ) -> anyhow::Result<()> {
        self.send_modify_mmio_range(
            crate::new_unblock_mmio_range_command(
                self.transport.bus_device_id(),
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

        let res = self.send_command(command).await?;

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
    /// # Arguments
    ///
    /// * `reason` - Reported to the host to explain why the TDI is unbinding.
    ///
    /// # Panics
    ///
    /// This function will panic if it fails to re-block any MMIO ranges or DMA
    /// that were previously unblocked. This ensures that the TDI is left in a
    /// consistent state after unblock.
    ///
    /// This function will also panic if the host lies about its acceptance of
    /// the unbind request. If the guest asking the trusted firmware disagrees
    /// with the state the host advertised after unbind, the function will
    /// panic.
    async fn unbind(&mut self, reason: TdispGuestUnbindReason) {
        let validator = self.resource_validator.clone();
        let device_id = self.mutable_state.guest_device_id;

        // If we haven't even made it far enough to know what TDI we're talking
        // to, we can't do any cleanup anyways.
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

                // Block the MMIO range again to return it to shared isolation.
                let block_mmio_res = validator
                    .tdisp_block_mmio(
                        self.target_vtl,
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
                    std::panic!("tdisp_unbind: failed to re-block MMIO range: {e}");
                }

                // Tell the host only once the platform actually blocked the
                // range, so the host can do any cleanup it needs to do for the
                // range.
                if let Err(e) = self
                    .host_block_mmio_range(bar_id, mmio.base_gpa, mmio.length_in_bytes)
                    .await
                {
                    tracing::error!(
                        bar_id,
                        base_gpa = format_args!("{:#x}", mmio.base_gpa),
                        length_in_bytes = mmio.length_in_bytes,
                        error = &*e as &dyn std::error::Error,
                        "tdisp_unbind: failed to block MMIO range on the host"
                    );
                    std::panic!("tdisp_unbind: failed to re-block MMIO range");
                }

                // Successful re-block, remove the bar from the validated list.
                self.mutable_state.validated_mmio_bars.remove(&bar_id);
            }

            if self.mutable_state.dma_unblocked {
                if let Err(e) = validator.tdisp_block_dma(self.target_vtl, raw_device_id) {
                    tracing::error!(
                        raw_device_id,
                        error = &*e as &dyn std::error::Error,
                        "tdisp_unbind: failed to re-block DMA"
                    );
                    std::panic!("tdisp_unbind: failed to re-block DMA: {e}");
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
            .send_command(crate::new_unbind_command(
                self.transport.bus_device_id(),
                reason,
            ))
            .await;

        if let Err(e) = res {
            tracing::error!(
                error = &*e as &dyn std::error::Error,
                "tdisp_unbind: error response from host"
            );
            std::panic!("tdisp_unbind: error response from host, cannot continue: {e}");
        }

        // The TDI must be back in Unlocked, and the firmware has to agree that
        // it is in Unlocked state as well. Any disagreement is fatal, as the state
        // the firmware sees must match the host's view.
        self.require_tdi_state(TdispTdiState::Unlocked, device_id);
    }

    /// Detects TDISP capabilities for the device. If the device supports TDISP
    /// and a guest protocol type that we support given the current VM's
    /// isolation level, then returns the interface info. Otherwise, returns an
    /// error representing why the device is not suitable for TDISP.
    async fn query_capabilities(&mut self) -> anyhow::Result<TdispDeviceInterfaceInfo> {
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
            .get_device_interface_info(target_protocol)
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

    /// Run the full attestation flow, leaving the TDI in Run with its interface
    /// report cached. Any prior attestation is torn down first, so this is safe
    /// to call from any TDI state.
    ///
    /// Resources are not yet accessible on return. They are unblocked when the
    /// guest enables MMIO, so that platform validation runs against the
    /// addresses the guest actually programmed.
    ///
    /// * `interface_info` - The negotiated capabilities for this device.
    async fn attest(&mut self, interface_info: TdispDeviceInterfaceInfo) -> anyhow::Result<()> {
        // Allow fast path if the device is already in `Run` state so that an entire attestation isn't run again.
        // Only allow this if the firmware specifically validates that the device is in the proper state before
        // continuing, else we risk a malicious host attempting to desync our state.
        if self.tdi_state() == TdispTdiState::Run {
            self.require_tdi_state(TdispTdiState::Run, self.mutable_state.guest_device_id);

            tracing::info!(
                "tdisp::attest: fast path: device already in `Run` state, skipping initial bind/attest cycle"
            );

            return Ok(());
        }

        let attestation_result = self.setup_and_attest(interface_info).await;

        match attestation_result {
            Ok(()) => {}
            Err(err) => {
                // Cleanup all partial resources on attestation failure.
                tracing::error!("attest: failed to attest device: {}", err.message);

                self.unbind(err.reason).await;

                return Err(anyhow::anyhow!(
                    "attest: failed to attest device: {}",
                    err.message
                ));
            }
        }

        Ok(())
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
    async fn setup_and_attest(
        &mut self,
        interface_info: TdispDeviceInterfaceInfo,
    ) -> Result<(), SetupDeviceFailure> {
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
            self.unbind(TdispGuestUnbindReason::Graceful).await;
        }

        // If there are *still* any attestation artifacts after unbind,
        // something went wrong. We can't continue out of paranoia.
        if self.tdi_state() != TdispTdiState::Unlocked
            || self.mutable_state.dma_unblocked
            || !self.mutable_state.validated_mmio_bars.is_empty()
        {
            return Err(SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: "tdisp_attest_device: failed to clear existing attestation state, cannot proceed with new attestation".to_string(),
            });
        }

        // The capability negotiation already identified the TDI, so take the id
        // from there rather than asking the host a second time. It is needed
        // before binding, so that the pre-bind and pre-start validator hooks
        // can identify the TDI they are gating.
        let guest_device_id = interface_info.tdisp_device_id;

        // Platforms require a u16 device ID even though the negotiated
        // interface info carries a u64. Ensure it fits within that constraint
        // before proceeding.
        let guest_device_id_u16 = u16::try_from(guest_device_id)
            .context("tdisp_attest_device: guest device ID must fit within u16")
            .map_err(|e| SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: format!(
                    "tdisp_attest_device: guest device ID must fit within u16: {}",
                    e
                ),
            })?;

        self.mutable_state
            .update_guest_device_id(TdispDeviceId::Valid(guest_device_id_u16));

        self.resource_validator
            .on_pre_bind(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: pre-bind validation failed")
            .map_err(|e| SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: format!("tdisp_attest_device: pre-bind validation failed: {}", e),
            })?;

        self.bind_interface()
            .await
            .context("tdisp_attest_device: failed to bind device interface")
            .map_err(|e| SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: format!(
                    "tdisp_attest_device: failed to bind device interface: {}",
                    e
                ),
            })?;

        self.require_tdi_state(
            TdispTdiState::Locked,
            TdispDeviceId::Valid(guest_device_id_u16),
        );

        self.resource_validator
            .on_pre_start(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: pre-start validation failed")
            .map_err(|e| SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: format!("tdisp_attest_device: pre-start validation failed: {}", e),
            })?;

        self.start_device()
            .await
            .context("tdisp_attest_device: failed to start device")
            .map_err(|e| SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: format!("tdisp_attest_device: failed to start device: {}", e),
            })?;

        self.require_tdi_state(
            TdispTdiState::Run,
            TdispDeviceId::Valid(guest_device_id_u16),
        );

        self.resource_validator
            .on_post_start(self.target_vtl, guest_device_id_u16)
            .context("tdisp_attest_device: post-start validation failed")
            .map_err(|e| SetupDeviceFailure {
                reason: TdispGuestUnbindReason::StartupFailure,
                message: format!("tdisp_attest_device: post-start validation failed: {}", e),
            })?;

        // Fetch and save the TDI interface report so callers can inspect the
        // attested device's reported capabilities and MMIO ranges.
        let tdi_report = self.get_tdi_report().await.context(
            "tdisp_attest_device: failed to get TDI interface report after starting device",
        ).map_err(|e| SetupDeviceFailure {
            reason: TdispGuestUnbindReason::StartupFailure,
            message: format!("tdisp_attest_device: failed to get TDI interface report after starting device: {}", e),
        })?;

        tracing::info!(
            ?tdi_report,
            %guest_device_id,
            "tdisp_attest_device: device attestation flow completed successfully, waiting on resources to be assigned"
        );

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

    /// Mark a BAR as being intercepted by the host. The classic case is the
    /// MSI-X table / PBA BAR which is handled by the hypervisor through MMIO
    /// enlightenments.
    ///
    /// Such a BAR is never made private, because it is not backed by RAM on
    /// the host and so has nothing that could be flipped.
    ///
    /// * `bar_id` - The PCI BAR index to mark.
    fn mark_bar_intercepted(&mut self, bar_id: u16) {
        if self.mutable_state.intercepted_bars.insert(bar_id) {
            tracing::info!(
                bar_id,
                "marking BAR as intercepted; TDISP MMIO unblock will be skipped for this BAR"
            );
        }
    }

    /// Classify a single BAR's isolation from the cached TDI interface report
    /// and the set of intercepted BARs.
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

        // Match range_id to BAR index and only report BARs that were physically
        // probed on the device.
        let Some(range) = report
            .mmio_interface_info
            .iter()
            .find(|r| r.range_id == bar_id)
        else {
            return if self.bar_masks[usize::from(bar_id)] {
                TdispResourceIsolation::Shared
            } else {
                TdispResourceIsolation::Invalid
            };
        };

        // `is_non_tee_mem` ranges report SHARED and are skipped by attestation.
        // Everything else is TEE memory the TDI owns -> PRIVATE.
        if range.flags.is_non_tee_mem() {
            TdispResourceIsolation::Shared
        } else {
            TdispResourceIsolation::Private
        }
    }

    /// Classify BAR and DMA isolation for this device at this instant in the
    /// flow.
    ///
    /// Returns `NotReady` iff no TDI interface report is currently cached.
    /// Callers that need a classification from an unattested device have to
    /// attest it first.
    fn isolation_snapshot(&self) -> TdispIsolationReport {
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
    /// device reports as non-TEE memory, BARs the paravisor has marked
    /// intercepted, and BARs the device implements but the report does not list
    /// are all skipped.
    ///
    /// Note: We have chosen to only allow MMIO reconfiguration only after the
    /// Run state is reached. This is an implementation decision.
    ///
    /// # Arguments
    ///
    /// * `bar_id` - The BAR index being configured. Matched against the
    ///   `range_id` of the MMIO ranges reported in the TDI interface report.
    /// * `base_address` - The base guest physical address of the MMIO range.
    /// * `length` - The length in bytes of the MMIO range.
    async fn on_mmio_reconfigured(
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
                let listed = self
                    .mutable_state
                    .tdi_report
                    .as_ref()
                    .is_some_and(|report| {
                        report
                            .mmio_interface_info
                            .iter()
                            .any(|r| r.range_id == bar_id)
                    });

                if listed {
                    tracing::info!(
                        bar_id,
                        base_address,
                        length,
                        "skipping MMIO unblock for BAR classified SHARED \
                         (intercepted or non-TEE memory)"
                    );
                } else {
                    tracing::info!(
                        bar_id,
                        base_address,
                        length,
                        "BAR is implemented by the device but has no entry in the TDI \
                         interface report; treating it as SHARED and skipping the MMIO unblock"
                    );
                }
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
                    "tdisp_on_mmio_reconfigured: BAR {bar_id} cannot be classified \
                     because no TDI interface report is cached; the device has not \
                     been attested"
                );
            }
            TdispResourceIsolation::Private => {}
        }

        // A device ID is required, this should be available if the device is in
        // Run. This error shouldn't happen in normal flows.
        let Some(device_id) = self.mutable_state.guest_device_id.id() else {
            anyhow::bail!(
                "tdisp_on_mmio_reconfigured: BAR {bar_id} is classified Private but no \
                 TDI device id is known"
            );
        };

        tracing::info!(
            "tdisp_on_mmio_reconfigured: unblocking MMIO for BAR classified PRIVATE: \
             device_id={device_id:#x}, bar_id={bar_id}, base_address={base_address:#x}, \
             length={length:#x}"
        );

        // Tell the host before the platform unblocks. Depending on the platform, the host
        // may need to perform bookkeeping operations before the guest can unblock the range.
        self.host_unblock_mmio_range(bar_id, base_address, length)
            .await
            .context("tdisp_on_mmio_reconfigured: failed to unblock MMIO on the host")?;

        self.resource_validator
            .tdisp_unblock_mmio(self.target_vtl, device_id, base_address, 0, length, bar_id)
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
        // unblock DMA as well. Guard with `dma_unblocked` so it only fires once
        // per bind/attest cycle (cleared on unbind).
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

#[cfg(test)]
mod tests {
    use super::*;

    use tdisp::devicereport::TdispTdiReportInterfaceInfo;
    use tdisp::devicereport::TdispTdiReportMmioFlags;
    use tdisp::devicereport::TdispTdiReportMmioInterfaceInfo;

    /// A transport for the classification tests, which never send a command.
    #[derive(Inspect)]
    struct PanickingTransport;

    impl TdispCommandTransport for PanickingTransport {
        fn bus_device_id(&self) -> u64 {
            0
        }

        fn send_command<'a>(
            &'a self,
            _command: GuestToHostCommand,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<GuestToHostResponse>> + Send + Sync + 'a>>
        {
            panic!("these tests must not send a TDISP command");
        }
    }

    /// Build a state whose `isolation_snapshot` and mutable-state fields are
    /// safe to poke directly.
    fn new_state() -> TdispClientState {
        new_state_with_bars([false; 6])
    }

    /// Build a state whose device implements the given BAR slots. Only the
    /// report-miss path consults this, so tests that never miss can use
    /// `new_state`.
    fn new_state_with_bars(present_bars: [bool; 6]) -> TdispClientState {
        TdispClientState::new(
            Box::new(PanickingTransport),
            Arc::new(crate::noop::TdispNoopResourceValidator::new()),
            IsolationType::None,
            Vtl::Vtl0,
            present_bars,
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
    fn unlisted_bar_is_shared_when_the_device_implements_it() {
        // The device has BARs 0 and 2; the report only describes BAR 0. BAR 2
        // is a real BAR the TDI does not claim, so it is host-visible, while
        // the slots the device does not implement stay unclassified.
        let mut state = new_state_with_bars([true, false, true, false, false, false]);
        state.mutable_state.tdi_report = Some(make_report(vec![tee_range(0)]));

        assert_eq!(state.classify_bar(0), TdispResourceIsolation::Private);
        assert_eq!(state.classify_bar(2), TdispResourceIsolation::Shared);
        for bar_id in [1, 3, 4, 5] {
            assert_eq!(
                state.classify_bar(bar_id),
                TdispResourceIsolation::Invalid,
                "bar {bar_id}"
            );
        }
    }

    #[test]
    fn unlisted_bar_reaches_the_guest_as_shared() {
        // The same device seen through the reply the guest actually receives.
        let mut state = new_state_with_bars([true, false, true, false, false, false]);
        state.mutable_state.tdi_report = Some(make_report(vec![tee_range(0)]));

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
                TdispResourceIsolation::Invalid,
                TdispResourceIsolation::Invalid,
            ]
        );
        assert_eq!(dma, TdispResourceIsolation::Private);
    }

    #[test]
    fn unlisted_bar_is_invalid_without_a_report() {
        // No report at all is the not-attested case, which stays unclassified
        // even for a BAR the device implements.
        let state = new_state_with_bars([true; 6]);
        assert_eq!(state.classify_bar(0), TdispResourceIsolation::Invalid);
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
