// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::process_loop::msg;
use super::process_loop::msg::IgvmAttestRequestData;
use crate::api::platform_settings;
use crate::api::GuestSaveRequest;
use chipset_resources::battery::HostBatteryUpdate;
use get_protocol::RegisterState;
use get_protocol::TripleFaultType;
use get_protocol::MAX_PAYLOAD_SIZE;
use guid::Guid;
use inspect::Inspect;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use std::cmp::min;
use std::sync::Arc;
use vpci::bus_control::VpciBusEvent;
use zerocopy::AsBytes;

/// Guest-side client for the GET.
///
/// A new client is created from [`spawn_get_worker`](crate::spawn_get_worker),
/// which initializes the GET worker and returns an instance of the client,
/// which can then be cloned to any objects / devices that need to communicate
/// over the GET.
#[derive(Inspect, Debug, Clone)]
pub struct GuestEmulationTransportClient {
    #[inspect(flatten)]
    control: Arc<ProcessLoopControl>,
    #[inspect(debug)]
    version: get_protocol::ProtocolVersion,
}

#[derive(Debug)]
struct ProcessLoopControl(mesh::Sender<msg::Msg>);

impl Inspect for ProcessLoopControl {
    fn inspect(&self, req: inspect::Request<'_>) {
        self.0.send(msg::Msg::Inspect(req.defer()));
    }
}

impl ProcessLoopControl {
    async fn call<I, R: 'static + Send>(
        &self,
        msg: impl FnOnce(Rpc<I, R>) -> msg::Msg,
        input: I,
    ) -> R {
        match self.0.call(msg, input).await {
            Ok(val) => val,
            // downstream clients are not expected to be resilient against the
            // GET going down. The only thing they can do in this case is
            // patiently wait for surrounding infrastructure to notice the GET
            // is down and start tearing everything down.
            Err(e) => {
                tracing::error!(
                    error = &e as &dyn std::error::Error,
                    "fatal error: GET process loop not available. waiting to get blown away..."
                );
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
    }

    fn notify(&self, msg: msg::Msg) {
        self.0.send(msg);
    }
}

pub struct ModifyVtl2SettingsRequest(
    pub Rpc<Vec<u8>, Result<(), Vec<underhill_config::Vtl2SettingsErrorInfo>>>,
);

impl GuestEmulationTransportClient {
    pub(crate) fn new(
        control: mesh::Sender<msg::Msg>,
        version: get_protocol::ProtocolVersion,
    ) -> GuestEmulationTransportClient {
        GuestEmulationTransportClient {
            control: Arc::new(ProcessLoopControl(control)),
            version,
        }
    }

    /// Queries the version cached from the version negotiation when
    /// initializing the GET worker
    pub fn version(&self) -> crate::api::ProtocolVersion {
        self.version
    }

    /// Sends a VMGS read request over the GET device
    ///
    /// # Arguments
    /// * `sector_offset` - Offset to start reading from the file
    /// * `buf` - Buffer to read the VMGS data into
    /// * `sector_size` - Size of a sector
    pub async fn vmgs_read(
        &self,
        sector_offset: u64,
        buf: &mut [u8],
        sector_size: usize,
    ) -> Result<(), crate::error::VmgsIoError> {
        let mut bytes_read = 0;

        while bytes_read < buf.len() {
            let data_len = min(buf.len() - bytes_read, MAX_PAYLOAD_SIZE);
            let sector_aligned_size = round_up_count(data_len, sector_size);

            let response_buf = self
                .control
                .call(
                    msg::Msg::VmgsRead,
                    msg::VmgsReadInput {
                        sector_offset: sector_offset + (bytes_read / sector_size) as u64,
                        length: sector_aligned_size / sector_size,
                        sector_size,
                    },
                )
                .await
                .map_err(|e| crate::error::VmgsIoError(e.status))?;

            buf[bytes_read..][..data_len].copy_from_slice(&response_buf[..data_len]);

            bytes_read += data_len;
        }

        Ok(())
    }

    /// Sends a VMGS write request over the GET device
    ///
    /// # Arguments
    /// * `sector_offset` - Offset to start reading from the file
    /// * `buf` - Buffer containing data being written to VMGS file.
    /// * `sector_size` - Size of a sector, must read entire sectors
    ///   over the GET
    pub async fn vmgs_write(
        &self,
        sector_offset: u64,
        buf: &[u8],
        sector_size: usize,
    ) -> Result<(), crate::error::VmgsIoError> {
        let mut bytes_written = 0;
        let buf_len = buf.len();

        while bytes_written < buf_len {
            let data_len = min(buf_len - bytes_written, MAX_PAYLOAD_SIZE);
            let sector_aligned_size = round_up_count(data_len, sector_size);

            let mut fragmented_buf = vec![0; sector_aligned_size];
            fragmented_buf[..data_len].copy_from_slice(&buf[bytes_written..][..data_len]);

            self.control
                .call(
                    msg::Msg::VmgsWrite,
                    msg::VmgsWriteInput {
                        sector_offset: sector_offset + (bytes_written / sector_size) as u64,
                        buf: fragmented_buf,
                        sector_size,
                    },
                )
                .await
                .map_err(|e| crate::error::VmgsIoError(e.status))?;

            bytes_written += data_len;
        }

        tracing::debug!("vmgs_write() successfully completed");
        Ok(())
    }

    /// Sends a VMGS get device info over the GET device
    pub async fn vmgs_get_device_info(
        &self,
    ) -> Result<crate::api::VmgsGetDeviceInfo, crate::error::VmgsIoError> {
        let response = self.control.call(msg::Msg::VmgsGetDeviceInfo, ()).await;

        if response.status != get_protocol::VmgsIoStatus::SUCCESS {
            return Err(crate::error::VmgsIoError(response.status));
        }

        Ok(crate::api::VmgsGetDeviceInfo {
            status: response.status,
            capacity: response.capacity,
            bytes_per_logical_sector: response.bytes_per_logical_sector,
            bytes_per_physical_sector: response.bytes_per_physical_sector,
            maximum_transfer_size_bytes: response.maximum_transfer_size_bytes,
        })
    }

    /// Sends a VMGS flush request over the GET device
    pub async fn vmgs_flush(&self) -> Result<(), crate::error::VmgsIoError> {
        let response = self.control.call(msg::Msg::VmgsFlush, ()).await;

        if response.status != get_protocol::VmgsIoStatus::SUCCESS {
            return Err(crate::error::VmgsIoError(response.status));
        }

        Ok(())
    }

    /// Retrieve Device Platform Settings using the new
    /// DEVICE_PLATFORM_SETTINGS_V2 packet (introduced in the Nickel GET
    /// protocol version)
    pub async fn device_platform_settings(
        &self,
    ) -> Result<platform_settings::DevicePlatformSettings, crate::error::DevicePlatformSettingsError>
    {
        let json = self
            .control
            .call(msg::Msg::DevicePlatformSettingsV2, ())
            .await;

        let json =
            serde_json::from_slice::<get_protocol::dps_json::DevicePlatformSettingsV2Json>(&json)
                .map_err(crate::error::DevicePlatformSettingsError::BadJson)?;

        let vtl2_settings = if let Some(settings) = &json.v2.r#static.vtl2_settings {
            Some(
                underhill_config::Vtl2Settings::read_from(settings, Default::default())
                    .map_err(crate::error::DevicePlatformSettingsError::BadVtl2Settings)?,
            )
        } else {
            None
        };

        Ok(platform_settings::DevicePlatformSettings {
            smbios: platform_settings::Smbios {
                serial_number: json.v1.serial_number.into(),
                base_board_serial_number: json.v1.base_board_serial_number.into(),
                chassis_serial_number: json.v1.chassis_serial_number.into(),
                chassis_asset_tag: json.v1.chassis_asset_tag.into(),

                system_manufacturer: json.v2.r#static.smbios.system_manufacturer.into(),
                system_product_name: json.v2.r#static.smbios.system_product_name.into(),
                system_version: json.v2.r#static.smbios.system_version.into(),
                system_sku_number: json.v2.r#static.smbios.system_sku_number.into(),
                system_family: json.v2.r#static.smbios.system_family.into(),
                bios_lock_string: json.v2.r#static.smbios.bios_lock_string.into(),
                memory_device_serial_number: json
                    .v2
                    .r#static
                    .smbios
                    .memory_device_serial_number
                    .into(),
                processor_manufacturer: json.v2.dynamic.smbios.processor_manufacturer,
                processor_version: json.v2.dynamic.smbios.processor_version,
                processor_id: json.v2.dynamic.smbios.processor_id,
                external_clock: json.v2.dynamic.smbios.external_clock,
                max_speed: json.v2.dynamic.smbios.max_speed,
                current_speed: json.v2.dynamic.smbios.current_speed,
                processor_characteristics: json.v2.dynamic.smbios.processor_characteristics,
                processor_family2: json.v2.dynamic.smbios.processor_family2,
                processor_type: json.v2.dynamic.smbios.processor_type,
                voltage: json.v2.dynamic.smbios.voltage,
                status: json.v2.dynamic.smbios.status,
                processor_upgrade: json.v2.dynamic.smbios.processor_upgrade,
            },
            general: platform_settings::General {
                secure_boot_enabled: json.v1.secure_boot_enabled,
                secure_boot_template: {
                    use crate::api::platform_settings::SecureBootTemplateType;
                    use get_protocol::dps_json::HclSecureBootTemplateId;

                    match json.v1.secure_boot_template_id {
                        HclSecureBootTemplateId::None => SecureBootTemplateType::None,
                        HclSecureBootTemplateId::MicrosoftWindows => {
                            SecureBootTemplateType::MicrosoftWindows
                        }
                        HclSecureBootTemplateId::MicrosoftUEFICertificateAuthority => {
                            SecureBootTemplateType::MicrosoftUefiCertificateAuthority
                        }
                    }
                },
                bios_guid: json.v1.bios_guid,
                console_mode: {
                    use crate::api::platform_settings::UefiConsoleMode;

                    match get_protocol::UefiConsoleMode(json.v1.console_mode) {
                        get_protocol::UefiConsoleMode::DEFAULT => UefiConsoleMode::Default,
                        get_protocol::UefiConsoleMode::COM1 => UefiConsoleMode::COM1,
                        get_protocol::UefiConsoleMode::COM2 => UefiConsoleMode::COM2,
                        get_protocol::UefiConsoleMode::NONE => UefiConsoleMode::None,
                        o => {
                            return Err(
                                crate::error::DevicePlatformSettingsError::InvalidConsoleMode(o),
                            )
                        }
                    }
                },
                battery_enabled: json.v1.enable_battery,
                processor_idle_enabled: json.v1.enable_processor_idle,
                tpm_enabled: json.v1.enable_tpm,
                com1_enabled: json.v1.com1.enable_port,
                com1_debugger_mode: json.v1.com1.debugger_mode,
                com1_vmbus_redirector: json.v1.com1.enable_vmbus_redirector,
                com2_enabled: json.v1.com2.enable_port,
                com2_debugger_mode: json.v1.com2.debugger_mode,
                com2_vmbus_redirector: json.v1.com2.enable_vmbus_redirector,
                firmware_debugging_enabled: json.v1.enable_firmware_debugging,
                hibernation_enabled: json.v1.enable_hibernation,

                suppress_attestation: Some(json.v2.r#static.no_persistent_secrets),
                generation_id: {
                    let mut gen_id = [0; 16];
                    gen_id[..8].copy_from_slice(&json.v2.dynamic.generation_id_low.to_ne_bytes());
                    gen_id[8..].copy_from_slice(&json.v2.dynamic.generation_id_high.to_ne_bytes());
                    Some(gen_id)
                },

                legacy_memory_map: json.v2.r#static.legacy_memory_map,
                pause_after_boot_failure: json.v2.r#static.pause_after_boot_failure,
                pxe_ip_v6: json.v2.r#static.pxe_ip_v6,
                measure_additional_pcrs: json.v2.r#static.measure_additional_pcrs,
                disable_frontpage: json.v2.r#static.disable_frontpage,
                disable_sha384_pcr: json.v2.r#static.disable_sha384_pcr,
                media_present_enabled_by_default: json.v2.r#static.media_present_enabled_by_default,
                vpci_boot_enabled: json.v2.r#static.vpci_boot_enabled,
                vpci_instance_filter: json.v2.r#static.vpci_instance_filter,
                memory_protection_mode: {
                    use crate::api::platform_settings::MemoryProtectionMode;

                    match json.v2.r#static.memory_protection_mode {
                        0b00 => MemoryProtectionMode::Disabled,
                        0b01 => MemoryProtectionMode::Default,
                        0b10 => MemoryProtectionMode::Strict,
                        0b11 => MemoryProtectionMode::Relaxed,
                        o => return Err(
                            crate::error::DevicePlatformSettingsError::InvalidMemoryProtectionMode(
                                o,
                            ),
                        ),
                    }
                },
                nvdimm_count: json.v2.dynamic.nvdimm_count,
                psp_enabled: json.v2.dynamic.enable_psp,
                vmbus_redirection_enabled: json.v2.r#static.vmbus_redirection_enabled,
                always_relay_host_mmio: json.v2.r#static.always_relay_host_mmio,
                vtl2_settings,
                watchdog_enabled: json.v2.r#static.watchdog_enabled,
                num_lock_enabled: json.v2.r#static.num_lock_enabled,
                pcat_boot_device_order: json.v2.r#static.pcat_boot_device_order.unwrap_or({
                    use crate::api::platform_settings::PcatBootDevice;
                    [
                        PcatBootDevice::Floppy,
                        PcatBootDevice::Optical,
                        PcatBootDevice::HardDrive,
                        PcatBootDevice::Network,
                    ]
                }),
                is_servicing_scenario: json.v2.dynamic.is_servicing_scenario,
                firmware_mode_is_pcat: json.v2.r#static.firmware_mode_is_pcat,
                imc_enabled: json.v2.r#static.imc_enabled,
            },
        })
    }

    /// Sends the host new content to encrypt and save content to decrypt
    pub async fn guest_state_protection_data(
        &self,
        encrypted_gsp: [crate::api::GspCiphertextContent; crate::api::NUMBER_GSP as usize],
        gsp_extended_status: crate::api::GspExtendedStatusFlags,
    ) -> crate::api::GuestStateProtection {
        let mut buffer = [0; get_protocol::GSP_CLEARTEXT_MAX as usize * 2];
        getrandom::getrandom(&mut buffer).expect("rng failure");

        let gsp_request = get_protocol::GuestStateProtectionRequest::new(
            buffer,
            encrypted_gsp,
            gsp_extended_status,
        );

        let response = self
            .control
            .call(msg::Msg::GuestStateProtection, Box::new(gsp_request))
            .await;

        crate::api::GuestStateProtection {
            encrypted_gsp: response.encrypted_gsp,
            decrypted_gsp: response.decrypted_gsp,
            extended_status_flags: response.extended_status_flags,
            new_gsp: gsp_request.new_gsp,
        }
    }

    /// Set the shared memory allocator, which is required by ['igvm_attest'].
    pub fn set_shared_memory_allocator(
        &mut self,
        shared_pool_allocator: shared_pool_alloc::SharedPoolAllocator,
        shared_guest_memory: guestmem::GuestMemory,
    ) {
        self.control.notify(msg::Msg::SetupSharedMemoryAllocator(
            shared_pool_allocator,
            shared_guest_memory,
        ));
    }

    /// Send the attestation request to the IGVM agent on the host.
    pub async fn igvm_attest(
        &self,
        agent_data: Vec<u8>,
        report: Vec<u8>,
    ) -> Result<crate::api::IgvmAttest, crate::error::IgvmAttestError> {
        let request = IgvmAttestRequestData { agent_data, report };

        let response = self
            .control
            .call(msg::Msg::IgvmAttest, Box::new(request))
            .await?;

        Ok(crate::api::IgvmAttest { response })
    }

    /// Sends a PowerOff notification back to the host.
    ///
    /// This function does not wait for a response from the host, since the host
    /// will terminate Underhill shortly after it receives the notification.
    pub fn send_power_off(&self) {
        tracing::info!("powering off...");
        self.control
            .notify(msg::Msg::PowerState(msg::PowerState::PowerOff));
    }

    /// Sends a Hibernate notification back to the host.
    ///
    /// This function does not wait for a response from the host, since the host
    /// will terminate Underhill shortly after it receives the notification.
    pub fn send_hibernate(&self) {
        tracing::info!("hibernating...");
        self.control
            .notify(msg::Msg::PowerState(msg::PowerState::Hibernate));
    }

    /// Sends a Reset notification back to the host.
    ///
    /// This function does not wait for a response from the host, since the host
    /// will terminate Underhill shortly after it receives the notification.
    pub fn send_reset(&self) {
        tracing::info!("resetting...");
        self.control
            .notify(msg::Msg::PowerState(msg::PowerState::Reset));
    }

    /// Customer facing event logging.
    ///
    /// This function is non-blocking and does not wait for a response from the
    /// host.
    ///
    /// When reporting fatal events (i.e: events which terminate underhill
    /// execution entirely), the caller must also await-on
    /// [`event_log_flush`](Self::event_log_flush) in order to ensure all queued
    /// events has actually been sent to the host.
    ///
    /// Not doing so may result in message loss due to the GET worker being
    /// shutdown prior to having processed all outstanding requests.
    pub fn event_log(&self, event_log_id: crate::api::EventLogId) {
        self.control.notify(msg::Msg::EventLog(event_log_id));
    }

    /// This async method will only resolve after all outstanding event logs
    /// are written back to the host.
    pub async fn event_log_flush(&self) {
        self.control.call(msg::Msg::FlushWrites, ()).await
    }

    /// Retrieves the current time from the host.
    pub async fn host_time(&self) -> crate::api::Time {
        let response = self.control.call(msg::Msg::HostTime, ()).await;
        crate::api::Time {
            utc: response.utc,
            time_zone: response.time_zone,
        }
    }

    /// Gets encryption seed from host.
    pub async fn guest_state_protection_data_by_id(
        &self,
    ) -> Result<crate::api::GuestStateProtectionById, crate::error::GuestStateProtectionByIdError>
    {
        let response = self
            .control
            .call(msg::Msg::GuestStateProtectionById, ())
            .await;

        if response.seed.length > response.seed.buffer.len() as u32 {
            return Err(crate::error::GuestStateProtectionByIdError(
                response.seed.length,
                response.seed.buffer.len() as u32,
            ));
        }

        Ok(crate::api::GuestStateProtectionById {
            seed: response.seed,
            extended_status_flags: response.extended_status_flags,
        })
    }

    /// Send start VTL0 complete notification to host.
    pub async fn complete_start_vtl0(&self, error_msg: Option<String>) {
        if self.version >= get_protocol::ProtocolVersion::NICKEL_REV2 {
            self.control
                .call(msg::Msg::CompleteStartVtl0, error_msg.clone())
                .await;

            if let Some(error_msg) = error_msg {
                // If we sent an error to the host, Underhill expects to be
                // terminated/halted. If this doesn't occur in 30 seconds, then
                // surface a panic to force a guest crash.
                mesh::CancelContext::new()
                    .with_timeout(std::time::Duration::from_secs(30))
                    .until_cancelled(std::future::pending::<()>())
                    .await
                    .unwrap_or_else(|_| {
                        panic!("should have been terminated after reporting start failure: {error_msg}")
                    });
            }
        }
    }

    /// Map the framebuffer
    pub async fn map_framebuffer(&self, gpa: u64) -> Result<(), crate::error::MapFramebufferError> {
        let response = self.control.call(msg::Msg::MapFramebuffer, gpa).await;
        match response.status {
            get_protocol::MapFramebufferStatus::SUCCESS => Ok(()),
            _ => Err(crate::error::MapFramebufferError(response.status)),
        }
    }

    /// Unmap the framebuffer
    pub async fn unmap_framebuffer(&self) -> Result<(), crate::error::UnmapFramebufferError> {
        let response = self.control.call(msg::Msg::UnmapFramebuffer, ()).await;
        match response.status {
            get_protocol::UnmapFramebufferStatus::SUCCESS => Ok(()),
            _ => Err(crate::error::UnmapFramebufferError(response.status)),
        }
    }

    /// Sends a message requesting the host to offer a VPCI device to this guest.
    pub async fn offer_vpci_device(
        &self,
        bus_instance_id: Guid,
    ) -> Result<(), crate::error::VpciControlError> {
        let response = self
            .control
            .call(
                msg::Msg::VpciDeviceControl,
                msg::VpciDeviceControlInput {
                    code: get_protocol::VpciDeviceControlCode::OFFER,
                    bus_instance_id,
                },
            )
            .await;
        if response.status != get_protocol::VpciDeviceControlStatus::SUCCESS {
            Err(crate::error::VpciControlError(response.status))
        } else {
            Ok(())
        }
    }

    /// Sends a message requesting the host to revoke a VPCI device to this guest.
    pub async fn revoke_vpci_device(
        &self,
        bus_instance_id: Guid,
    ) -> Result<(), crate::error::VpciControlError> {
        let response = self
            .control
            .call(
                msg::Msg::VpciDeviceControl,
                msg::VpciDeviceControlInput {
                    code: get_protocol::VpciDeviceControlCode::REVOKE,
                    bus_instance_id,
                },
            )
            .await;
        if response.status != get_protocol::VpciDeviceControlStatus::SUCCESS {
            Err(crate::error::VpciControlError(response.status))
        } else {
            Ok(())
        }
    }

    /// Sends a message to the host reporting a VPCI device binding state change.
    pub async fn report_vpci_device_binding_state(
        &self,
        bus_instance_id: Guid,
        binding_state: bool,
    ) -> Result<(), crate::error::VpciControlError> {
        let response = self
            .control
            .call(
                msg::Msg::VpciDeviceBindingChange,
                msg::VpciDeviceBindingChangeInput {
                    bus_instance_id,
                    binding_state,
                },
            )
            .await;
        if response.status != get_protocol::VpciDeviceControlStatus::SUCCESS {
            Err(crate::error::VpciControlError(response.status))
        } else {
            Ok(())
        }
    }

    /// Creates a listener (in the form of an `UnboundedReceiver`) that receives
    /// notifications for the specified VPCI device.
    pub async fn connect_to_vpci_event_source(
        &self,
        bus_instance_id: Guid,
    ) -> mesh::Receiver<VpciBusEvent> {
        let (sender, receiver) = mesh::channel();
        self.control
            .call(
                msg::Msg::VpciListenerRegistration,
                msg::VpciListenerRegistrationInput {
                    bus_instance_id,
                    sender,
                },
            )
            .await;
        receiver
    }

    /// Disconnects a listener from the specified VPCI device.
    pub fn disconnect_from_vpci_event_source(&self, bus_instance_id: Guid) {
        self.control
            .notify(msg::Msg::VpciListenerDeregistration(bus_instance_id));
    }

    /// Take the vtl2 settings recv channel. Returns `None` if the channel has already been taken.
    pub async fn take_vtl2_settings_recv(
        &self,
    ) -> Option<mesh::Receiver<ModifyVtl2SettingsRequest>> {
        self.control
            .call(msg::Msg::TakeVtl2SettingsReceiver, ())
            .await
    }

    /// Take the generation id recv channel. Returns `None` if the channel has already been taken.
    pub async fn take_generation_id_recv(&self) -> Option<mesh::Receiver<[u8; 16]>> {
        self.control.call(msg::Msg::TakeGenIdReceiver, ()).await
    }

    /// Take the battery status recv channel. Returns 'None' if the channel has already been taken.
    pub async fn take_battery_status_recv(&self) -> Option<mesh::Receiver<HostBatteryUpdate>> {
        self.control
            .call(msg::Msg::TakeBatteryStatusReceiver, ())
            .await
    }

    /// Read a PCI config space value from the proxied VGA device.
    pub async fn vga_proxy_pci_read(&self, offset: u16) -> u32 {
        let response = self.control.call(msg::Msg::VgaProxyPciRead, offset).await;
        response.value
    }

    /// Write a PCI config space value to the proxied VGA device.
    pub async fn vga_proxy_pci_write(&self, offset: u16, value: u32) {
        self.control
            .call(
                msg::Msg::VgaProxyPciWrite,
                msg::VgaProxyPciWriteInput { offset, value },
            )
            .await;
    }

    /// Invokes `IVmGuestMemoryAccess::CreateRamGpaRange` on the host
    pub async fn create_ram_gpa_range(
        &self,
        slot: u32,
        gpa_start: u64,
        gpa_count: u64,
        gpa_offset: u64,
        flags: crate::api::CreateRamGpaRangeFlags,
    ) -> Result<crate::api::RemoteRamGpaRangeHandle, crate::error::CreateRamGpaRangeError> {
        let response = self
            .control
            .call(
                msg::Msg::CreateRamGpaRange,
                msg::CreateRamGpaRangeInput {
                    slot,
                    gpa_start,
                    gpa_count,
                    gpa_offset,
                    flags,
                },
            )
            .await;
        if response.status != get_protocol::CreateRamGpaRangeStatus::SUCCESS {
            Err(crate::error::CreateRamGpaRangeError(response.status))
        } else {
            Ok(crate::api::RemoteRamGpaRangeHandle::from_raw(slot))
        }
    }

    /// Invokes `.Reset()` on host object corresponding to a handle returned by
    /// `CreateRamGpaHandle`
    pub async fn reset_ram_gpa_range(&self, handle: crate::api::RemoteRamGpaRangeHandle) {
        self.control
            .call(msg::Msg::ResetRamGpaRange, handle.as_raw())
            .await;
    }

    /// Gets the saved state from the host. Returns immediately with whatever
    /// saved state existed at the time the call is processed, which may be None.
    pub async fn get_saved_state_from_host(
        &self,
    ) -> Result<Vec<u8>, crate::error::SaveRestoreOperationFailure> {
        self.control
            .call(msg::Msg::GetVtl2SavedStateFromHost, ())
            .await
            .map_err(|()| crate::error::SaveRestoreOperationFailure {})
    }

    /// Reports the result of a restore operation to the host.
    /// Limited to reporting either success or failure.
    /// TODO: consider adding an error code or similar
    /// to increase reporting ability/host-side diagnosability.
    pub async fn report_restore_result_to_host(&self, success: bool) {
        self.control
            .notify(msg::Msg::ReportRestoreResultToHost(success));
    }

    /// Take the save request receiver, which allows the VM to respond to
    /// host-sent notifications to save state. Returns `None` if the channel has
    /// already been taken.
    pub async fn take_save_request_recv(&self) -> Option<mesh::Receiver<GuestSaveRequest>> {
        self.control
            .call(msg::Msg::TakeSaveRequestReceiver, ())
            .await
    }

    /// Sends servicing state to the host.
    ///
    /// This should only be called when servicing state has been requested via
    /// the channel returned by [`Self::take_save_request_recv`].
    pub async fn send_servicing_state(
        &self,
        data: Vec<u8>,
    ) -> Result<(), crate::error::SaveRestoreOperationFailure> {
        self.control
            .call(msg::Msg::SendServicingState, Ok(data))
            .await
            .map_err(|()| crate::error::SaveRestoreOperationFailure {})
    }

    /// Sends a servicing failure to the host.
    ///
    /// This should only be called when servicing state has been requested via
    /// the channel returned by [`Self::take_save_request_recv`].
    pub async fn send_servicing_failure(
        &self,
        err: impl ToString,
    ) -> Result<(), crate::error::SaveRestoreOperationFailure> {
        self.control
            .call(msg::Msg::SendServicingState, Err(err.to_string()))
            .await
            .map_err(|()| crate::error::SaveRestoreOperationFailure {})
    }

    /// Notify of a VTL crash
    pub fn notify_of_vtl_crash(
        &self,
        vp_index: u32,
        last_vtl: u8,
        control: u64,
        parameters: [u64; get_protocol::VTL_CRASH_PARAMETERS],
    ) {
        self.control.notify(msg::Msg::VtlCrashNotification(
            get_protocol::VtlCrashNotification::new(vp_index, last_vtl, control, parameters),
        ));
    }

    /// Notify of a triple fault.
    pub fn triple_fault(
        &self,
        vp_index: u32,
        fault_type: TripleFaultType,
        reg_state: Vec<RegisterState>,
    ) {
        let mut payload = vec![];

        let notification = get_protocol::TripleFaultNotification::new(
            vp_index,
            fault_type,
            reg_state.len() as u32,
        );
        payload.extend_from_slice(notification.as_bytes());
        payload.extend_from_slice(reg_state.as_bytes());

        self.control
            .notify(msg::Msg::TripleFaultNotification(payload));
    }
}

fn round_up_count(count: usize, pow2: usize) -> usize {
    (count + pow2 - 1) & !(pow2 - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_up_count() {
        assert!(round_up_count(0, 4096) == 0);
        assert!(round_up_count(1, 4096) == 4096);
        assert!(round_up_count(4095, 4096) == 4096);
        assert!(round_up_count(4096, 4096) == 4096);
        assert!(round_up_count(4097, 4096) == 8192);
    }
}
