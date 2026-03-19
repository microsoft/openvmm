// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDISP interface implementation for VPCI devices.

use anyhow::Context;
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

#[derive(Inspect)]
struct VpciClientTdispMutableState {
    tdi_state: TdispTdiState,
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
}

/// TDISP state for a VPCI device.
#[derive(Inspect)]
pub struct VpciClientTdispState {
    #[inspect(skip)]
    worker_req: mesh::Sender<WorkerRequest>,
    device_id: u64,
    isolation_type: IsolationType,
    vtom: u64,
    mutable_state: VpciClientTdispMutableState,
}

/// Manages the TDISP protocol for a TDISP-capable VPCI device.
impl VpciClientTdispState {
    pub(super) fn new(
        worker_req: mesh::Sender<WorkerRequest>,
        device_id: u64,
        isolation_type: IsolationType,
        vtom: u64,
    ) -> Self {
        Self {
            worker_req,
            device_id,
            mutable_state: VpciClientTdispMutableState {
                tdi_state: TdispTdiState::Uninitialized,
            },
            isolation_type,
            vtom,
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
                        slot: SlotNumber::from_bits(self.device_id as u32),
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
                self.device_id,
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
            .send_tdisp_command(openhcl_tdisp::new_bind_command(self.device_id))
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
            .send_tdisp_command(openhcl_tdisp::new_start_tdi_command(self.device_id))
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
                self.device_id,
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
            .send_tdisp_command(openhcl_tdisp::new_unbind_command(self.device_id, reason))
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

        // Device is now in the Run state without resource validation being performed.
        // Platform specific validation methods should be called to unblock resources.
        Ok(())
    }

    /// Get the TDI state of the device. This is used for testing and validation purposes, and is not part of the standard TDISP flow.
    pub fn tdisp_get_tdi_state(&self) -> TdispTdiState {
        self.tdi_state()
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
}
