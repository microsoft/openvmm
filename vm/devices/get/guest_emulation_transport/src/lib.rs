// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest Emulation Transport - GET
//!
//! The GET is the guest side of a communication channel that uses VMBUS to communicate between Guest and Host.
//! The Guest sends messages through the GET to get information on the time, VMGS file, attestation,
//! platform settings, bios boot settings, and guest state protection.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

pub mod api;
pub mod error;
pub mod resolver;

mod client;
mod process_loop;
mod worker;

pub use client::GuestEmulationTransportClient;

/// Error while initialize the GET worker
#[derive(Debug, thiserror::Error)]
#[error("failed to initialize GET worker")]
pub struct SpawnGetError(#[source] process_loop::FatalError);

/// Encountered fatal GET error
// DEVNOTE: this is a distinct type from `process_loop::FatalError`, as we don't
// want to leak the details of the internal FatalError type.
#[derive(Debug, thiserror::Error)]
#[error("encountered fatal GET error")]
pub struct FatalGetError(#[source] process_loop::FatalError);

/// Takes in a driver and initializes the GET, returning a client that can be
/// used to invoke requests to the GET worker.
pub async fn spawn_get_worker(
    driver: impl pal_async::driver::SpawnDriver,
) -> Result<
    (
        GuestEmulationTransportClient,
        pal_async::task::Task<Result<(), FatalGetError>>,
    ),
    SpawnGetError,
> {
    let (worker, task) = worker::GuestEmulationTransportWorker::new(driver)
        .await
        .map_err(SpawnGetError)?;
    Ok((worker.new_client(), task))
}

#[cfg(any(feature = "test_utilities", test))]
#[expect(missing_docs)]
pub mod test_utilities {
    use super::*;
    use crate::worker::GuestEmulationTransportWorker;
    use client::GuestEmulationTransportClient;
    use get_protocol::ProtocolVersion;
    use guest_emulation_device::IgvmAgentTestPlan;
    use guest_emulation_device::test_utilities::TestGedClient;
    use guest_emulation_device::test_utilities::TestGetResponses;
    use mesh::Receiver;
    use pal_async::task::Spawn;
    use pal_async::task::Task;

    pub const DEFAULT_SIZE: usize = 4194816; // 4 MB

    #[cfg_attr(not(test), expect(dead_code))]
    pub struct TestGet {
        pub client: GuestEmulationTransportClient,
        pub(crate) gen_id: Receiver<[u8; 16]>,
        pub(crate) guest_task: Task<Result<(), FatalGetError>>,
        pub(crate) test_ged_client: TestGedClient,
    }

    /// Creates a new host guest transport pair ready to send data.
    ///
    /// If `ged_responses` is Some(), then TestGedChannel will be used to
    /// control what responses the Host sends. Otherwise, if `ged_responses` is
    /// None, we will use the regular GedChannel to automate responses.
    pub async fn new_transport_pair(
        spawn: impl Spawn,
        ged_responses: Option<Vec<TestGetResponses>>,
        version: ProtocolVersion,
        guest_memory: Option<guestmem::GuestMemory>,
        igvm_agent_script: Option<IgvmAgentTestPlan>,
    ) -> TestGet {
        let (host_vmbus, guest_vmbus) = vmbus_async::pipe::connected_message_pipes(
            get_protocol::MAX_MESSAGE_SIZE + vmbus_ring::PAGE_SIZE,
        );

        let test_ged_client = guest_emulation_device::test_utilities::create_host_channel(
            &spawn,
            host_vmbus,
            ged_responses,
            version,
            guest_memory,
            igvm_agent_script,
        );

        // Create the GET
        let (guest_transport, guest_task) =
            GuestEmulationTransportWorker::with_pipe(&spawn, guest_vmbus)
                .await
                .unwrap();

        let client = guest_transport.new_client();

        TestGet {
            gen_id: client.take_generation_id_recv().await.unwrap(),
            client,
            guest_task,
            test_ged_client,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_utilities::*;
    use super::worker::GuestEmulationTransportWorker;
    use crate::process_loop::FatalError;
    use get_protocol::ProtocolVersion;
    use get_protocol::VmgsIoStatus;
    use get_protocol::test_utilities::TEST_VMGS_SECTOR_SIZE;
    use guest_emulation_device::test_utilities::Event;
    use guest_emulation_device::test_utilities::TestGetResponses;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use test_with_tracing::test;
    use vmbus_async::async_dgram::AsyncRecvExt;
    use vmbus_async::async_dgram::AsyncSendExt;
    use zerocopy::FromZeros;
    use zerocopy::IntoBytes;

    #[async_test]
    async fn test_version_negotiation_failed(driver: DefaultDriver) {
        let (mut host_vmbus, guest_vmbus) =
            vmbus_async::pipe::connected_message_pipes(get_protocol::MAX_MESSAGE_SIZE);

        let host_task = driver.spawn("host task", async move {
            for protocol in [ProtocolVersion::NICKEL_REV2] {
                let mut version_request = get_protocol::VersionRequest::new_zeroed();
                let len = version_request.as_bytes().len();
                assert_eq!(
                    len,
                    host_vmbus
                        .recv(version_request.as_mut_bytes())
                        .await
                        .unwrap()
                );

                assert_eq!(
                    version_request.message_header.message_id(),
                    get_protocol::HostRequests::VERSION
                );
                assert_eq!(version_request.version, protocol);

                // Reject the request.
                let version_response = get_protocol::VersionResponse::new(false);

                host_vmbus.send(version_response.as_bytes()).await.unwrap();
            }
        });

        let transport = GuestEmulationTransportWorker::with_pipe(driver, guest_vmbus).await;

        match transport.unwrap_err() {
            FatalError::VersionNegotiationFailed => {}
            e => panic!("Wrong error type returned: {}", e),
        }

        host_task.await;
    }

    #[async_test]
    async fn test_all_basic(driver: DefaultDriver) {
        let time_zone = 5;
        let utc = 3;

        let time_response = TestGetResponses::new(Event::Response(
            get_protocol::TimeResponse::new(0, utc, time_zone, false)
                .as_bytes()
                .to_vec(),
        ));

        let vmgs_device_info_response = TestGetResponses::new(Event::Response(
            get_protocol::VmgsGetDeviceInfoResponse::new(VmgsIoStatus::SUCCESS, 1, 2, 3, 4)
                .as_bytes()
                .to_vec(),
        ));

        let flush_response = TestGetResponses::new(Event::Response(
            get_protocol::VmgsFlushResponse::new(VmgsIoStatus::SUCCESS)
                .as_bytes()
                .to_vec(),
        ));

        let guest_state_protection = TestGetResponses::new(Event::Response(
            get_protocol::GuestStateProtectionResponse {
                message_header: get_protocol::HeaderGeneric::new(
                    get_protocol::HostRequests::GUEST_STATE_PROTECTION,
                ),
                encrypted_gsp: get_protocol::GspCiphertextContent::new_zeroed(),
                decrypted_gsp: [get_protocol::GspCleartextContent::new_zeroed();
                    get_protocol::NUMBER_GSP as usize],
                extended_status_flags: get_protocol::GspExtendedStatusFlags::new()
                    .with_state_refresh_request(true),
            }
            .as_bytes()
            .to_vec(),
        ));

        let gsp_id = TestGetResponses::new(Event::Response(
            get_protocol::GuestStateProtectionByIdResponse {
                message_header: get_protocol::HeaderGeneric::new(
                    get_protocol::HostRequests::GUEST_STATE_PROTECTION_BY_ID,
                ),
                seed: get_protocol::GspCleartextContent::new_zeroed(),
                extended_status_flags: get_protocol::GspExtendedStatusFlags::new()
                    .with_no_registry_file(true)
                    .with_state_refresh_request(true),
            }
            .as_bytes()
            .to_vec(),
        ));

        let igvm_attest = TestGetResponses::new(Event::Response(
            get_protocol::IgvmAttestResponse {
                message_header: get_protocol::HeaderGeneric::new(
                    get_protocol::HostRequests::IGVM_ATTEST,
                ),
                length: 512,
            }
            .as_bytes()
            .to_vec(),
        ));

        let ged_responses = vec![
            time_response,
            vmgs_device_info_response,
            flush_response,
            guest_state_protection,
            gsp_id,
            igvm_attest,
        ];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let result = get.client.host_time().await;

        assert_eq!(result.utc, utc);
        assert_eq!(result.time_zone, time_zone);

        let response = get.client.vmgs_get_device_info().await.unwrap();
        assert_eq!(response.capacity, 1);
        assert_eq!(response.bytes_per_logical_sector, 2);
        assert_eq!(response.bytes_per_physical_sector, 3);
        assert_eq!(response.maximum_transfer_size_bytes, 4);

        get.client.vmgs_flush().await.unwrap();

        let gsp_response = get
            .client
            .guest_state_protection_data(
                [get_protocol::GspCiphertextContent::new_zeroed();
                    get_protocol::NUMBER_GSP as usize],
                get_protocol::GspExtendedStatusFlags::new().with_state_refresh_request(true),
            )
            .await;

        assert_eq!(
            gsp_response.extended_status_flags,
            get_protocol::GspExtendedStatusFlags::new().with_state_refresh_request(true)
        );

        let gsp_id_response = get
            .client
            .guest_state_protection_data_by_id()
            .await
            .unwrap();

        assert_eq!(
            gsp_id_response.extended_status_flags,
            get_protocol::GspExtendedStatusFlags::new()
                .with_no_registry_file(true)
                .with_state_refresh_request(true)
        );
    }

    #[async_test]
    async fn test_vmgs_basic_write(driver: DefaultDriver) {
        let vmgs_write_response = TestGetResponses::new(Event::Response(
            get_protocol::VmgsWriteResponse::new(VmgsIoStatus::SUCCESS)
                .as_bytes()
                .to_vec(),
        ));

        let vmgs_read_response = TestGetResponses::new(Event::Response(
            get_protocol::VmgsReadResponse::new(VmgsIoStatus::SUCCESS)
                .as_bytes()
                .to_vec(),
        ));
        let ged_responses = vec![vmgs_write_response, vmgs_read_response];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;
        let buf = (0..512).map(|x| x as u8).collect::<Vec<u8>>();
        get.client
            .vmgs_write(0, buf.clone(), TEST_VMGS_SECTOR_SIZE)
            .await
            .unwrap();

        let read_buf = get
            .client
            .vmgs_read(0, 1, TEST_VMGS_SECTOR_SIZE)
            .await
            .unwrap();
        assert_eq!(read_buf, buf);
    }

    #[async_test]
    async fn different_get_versions_nickel(driver: DefaultDriver) {
        // NICKEL, json dps no aps
        let json = get_protocol::dps_json::DevicePlatformSettingsV2Json {
            v1: get_protocol::dps_json::HclDevicePlatformSettings {
                com1: get_protocol::dps_json::HclUartSettings {
                    enable_port: true,
                    debugger_mode: false,
                    enable_vmbus_redirector: true,
                    tx_only: false,
                },
                com2: get_protocol::dps_json::HclUartSettings {
                    enable_port: false,
                    debugger_mode: false,
                    enable_vmbus_redirector: false,
                    tx_only: false,
                },
                enable_firmware_debugging: true,
                ..Default::default()
            },
            v2: get_protocol::dps_json::HclDevicePlatformSettingsV2 {
                r#static: get_protocol::dps_json::HclDevicePlatformSettingsV2Static {
                    legacy_memory_map: true,
                    pxe_ip_v6: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        };
        let json_data = serde_json::to_vec(&json).unwrap();

        let mut dps_response = get_protocol::DevicePlatformSettingsResponseV2Rev1::new_zeroed();
        dps_response.message_header = get_protocol::HeaderGeneric::new(
            get_protocol::HostRequests::DEVICE_PLATFORM_SETTINGS_V2_REV1,
        );
        dps_response.size = json_data.len() as u32;
        dps_response.payload_state = get_protocol::LargePayloadState::END;

        let device_platform_settings = TestGetResponses::new(Event::Response(
            [dps_response.as_bytes().to_vec(), json_data].concat(),
        ));

        let ged_responses = vec![device_platform_settings];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let dps = get.client.device_platform_settings().await.unwrap();
        assert_eq!(dps.general.tpm_enabled, false);
        assert_eq!(dps.general.com1_enabled, true);
        assert_eq!(dps.general.secure_boot_enabled, false);

        assert_eq!(dps.general.legacy_memory_map, true);
        assert_eq!(dps.general.pxe_ip_v6, true);
        assert_eq!(dps.general.nvdimm_count, 0);

        assert_eq!(dps.general.generation_id, Some([0; 16]));
    }

    #[async_test]
    async fn test_send_notification(driver: DefaultDriver) {
        // HACK: host notifications are programmed to set the first byte in the
        // vmgs to a certain value.
        let vmgs_read_response = TestGetResponses::new(Event::Response(
            get_protocol::VmgsReadResponse::new(VmgsIoStatus::SUCCESS)
                .as_bytes()
                .to_vec(),
        ));
        let ged_responses = vec![TestGetResponses::default(), vmgs_read_response];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        get.client
            .event_log(get_protocol::EventLogId::NO_BOOT_DEVICE);

        let read_buf = get
            .client
            .vmgs_read(0, 1, TEST_VMGS_SECTOR_SIZE)
            .await
            .unwrap();

        assert_eq!(read_buf[0], 5);
    }

    #[async_test]
    async fn notification_in_between_requests(driver: DefaultDriver) {
        let time_response = TestGetResponses::new(Event::Response(
            get_protocol::UpdateGenerationId::new([1; 16])
                .as_bytes()
                .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ));

        let ged_responses = vec![time_response];

        let mut get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let result = get.client.host_time().await;

        let gen_id = get.gen_id.recv().await.unwrap();

        assert_eq!(gen_id, [1; 16]);

        assert_eq!(result.utc, 1);
        assert_eq!(result.time_zone, 2);
    }

    #[async_test]
    async fn host_send_multiple_response(driver: DefaultDriver) {
        let time_response = TestGetResponses::new(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ));

        let ged_responses = vec![time_response];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let result = get.client.host_time().await;

        assert_eq!(result.utc, 1);
        assert_eq!(result.time_zone, 2);

        // The GET can tolerate extraneous responses, ensure it doesn't crash
        assert!(futures::poll!(get.guest_task).is_pending());
    }

    #[async_test]
    async fn host_send_mismatched_multiple_response(driver: DefaultDriver) {
        let responses = TestGetResponses::new(Event::Response(
            get_protocol::VpciDeviceControlResponse::new(
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::VpciDeviceControlResponse::new(
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ));

        let ged_responses = vec![responses];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let time_req = get.client.host_time();

        let result = time_req.await;
        assert_eq!(result.utc, 1);
        assert_eq!(result.time_zone, 2);
    }

    #[async_test]
    async fn host_send_mismatched_multiple_request_response(driver: DefaultDriver) {
        let responses = TestGetResponses::new(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::VpciDeviceControlResponse::new(
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ))
        .add_response(Event::Response(
            get_protocol::VpciDeviceControlResponse::new(
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ));

        let ged_responses = vec![responses];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let time_req = get.client.host_time();
        let mut vpci_req = std::pin::pin!(get.client.offer_vpci_device(guid::Guid::new_random()));

        // Start the VPCI request going so it enters the queue.
        assert!(futures::poll!(&mut vpci_req).is_pending());

        // Run the full time request, ensuring it can handle the other responses.
        let result = time_req.await;
        assert_eq!(result.utc, 1);
        assert_eq!(result.time_zone, 2);

        // Now let the VPCI request finish with one of the responses.
        vpci_req.await.unwrap();
    }

    #[async_test]
    async fn host_send_incorrect_response(driver: DefaultDriver) {
        let time_response = TestGetResponses::new(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ));

        let ged_responses = vec![time_response];

        let get = new_transport_pair(
            driver.clone(),
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let _never_returns = driver.spawn("badness", async move {
            let _ = get.client.vmgs_get_device_info().await;
        });

        let internal_error = get.guest_task.await;

        assert!(matches!(
            internal_error.map_err(|x| x.0),
            Err(FatalError::ResponseHeaderMismatchId(_, _))
        ));
    }

    #[async_test]
    async fn test_send_halt_reason(driver: DefaultDriver) {
        let power_off_check =
            TestGetResponses::new(Event::Halt(power_resources::PowerRequest::PowerOff));
        let reset_check = TestGetResponses::new(Event::Halt(power_resources::PowerRequest::Reset));

        let vmgs_device_info_response = TestGetResponses::new(Event::Response(
            get_protocol::VmgsGetDeviceInfoResponse::new(VmgsIoStatus::SUCCESS, 1, 2, 3, 4)
                .as_bytes()
                .to_vec(),
        ));

        let ged_responses = vec![power_off_check, reset_check, vmgs_device_info_response];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        get.client.send_power_off();
        get.client.send_reset();

        // We send a vmgs_get_device_info() so we can ensure the host
        // finishes processing the notifications we previously sent.
        // Otherwise, the socket may close before the host can finish
        // handling both notifications.
        let response = get.client.vmgs_get_device_info().await.unwrap();
        assert_eq!(response.capacity, 1);
        assert_eq!(response.bytes_per_logical_sector, 2);
        assert_eq!(response.bytes_per_physical_sector, 3);
        assert_eq!(response.maximum_transfer_size_bytes, 4);
    }

    #[async_test]
    async fn test_send_multiple_host_request(driver: DefaultDriver) {
        let time_response = TestGetResponses::new(Event::Response(
            get_protocol::TimeResponse::new(0, 1, 2, false)
                .as_bytes()
                .to_vec(),
        ));

        let ged_responses = vec![
            time_response.clone(),
            time_response.clone(),
            time_response.clone(),
            time_response.clone(),
            time_response.clone(),
            time_response,
        ];

        let get = new_transport_pair(
            driver.clone(),
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;

        let mut tasks = Vec::new();

        for i in 0..6 {
            let client = get.client.clone();
            tasks.push(driver.spawn(
                format!("task {}", i),
                async move { client.host_time().await },
            ));
        }

        // Sleep 1 second to let the host process tasks
        // std::thread::sleep(std::time::Duration::new(1, 0));

        for task in tasks {
            let time = task.await;
            assert_eq!(time.utc, 1);
            assert_eq!(time.time_zone, 2);
        }
    }
    #[async_test]
    async fn test_vpci_control(driver: DefaultDriver) {
        let bus_id = guid::Guid::new_random();
        let vpci_offer_response = TestGetResponses::new(Event::Response(
            get_protocol::VpciDeviceControlResponse::new(
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ));

        let vpci_revoke_response = TestGetResponses::new(Event::Response(
            get_protocol::VpciDeviceControlResponse::new(
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ));

        let vpci_bind_response = TestGetResponses::new(Event::Response(
            get_protocol::VpciDeviceBindingChangeResponse::new(
                bus_id,
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ));

        let vpci_unbind_response = TestGetResponses::new(Event::Response(
            get_protocol::VpciDeviceBindingChangeResponse::new(
                bus_id,
                get_protocol::VpciDeviceControlStatus::SUCCESS,
            )
            .as_bytes()
            .to_vec(),
        ));

        let ged_responses = vec![
            vpci_offer_response,
            vpci_revoke_response,
            vpci_bind_response,
            vpci_unbind_response,
        ];

        let get = new_transport_pair(
            driver,
            Some(ged_responses),
            ProtocolVersion::NICKEL_REV2,
            None,
            None,
        )
        .await;
        get.client.offer_vpci_device(bus_id).await.unwrap();
        get.client.revoke_vpci_device(bus_id).await.unwrap();
        get.client
            .report_vpci_device_binding_state(bus_id, true)
            .await
            .unwrap();
        get.client
            .report_vpci_device_binding_state(bus_id, false)
            .await
            .unwrap();

        get.client.connect_to_vpci_event_source(bus_id).await;
        get.client.disconnect_from_vpci_event_source(bus_id);
    }

    // Temporarily ignored until error handling is done better/OpenVMM as host flow is plumbed in.
    #[ignore]
    #[async_test]
    async fn test_save_guest_vtl2_state(driver: DefaultDriver) {
        let mut get =
            new_transport_pair(driver, None, ProtocolVersion::NICKEL_REV2, None, None).await;

        get.test_ged_client.test_save_guest_vtl2_state().await;
    }
}
