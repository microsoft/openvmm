// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ScsiDisk Persistent Reserve tests.

use super::test_helpers::check_execute_scsi_failed_with_result;
use super::test_helpers::check_execute_scsi_pass;
use super::test_helpers::check_execute_scsi_pass_with_tx;
use super::test_helpers::check_guest_memory;
use super::test_helpers::make_guest_memory;
use super::test_helpers::new_scsi_disk;
use crate::scsi;
use crate::SimpleScsiDisk;
use disk_backend::pr;
use guestmem::GuestMemory;
use pal_async::async_test;
use scsi::srb::SrbStatus;
use scsi::AdditionalSenseCode;
use scsi::ScsiStatus;
use scsi::SenseData;
use scsi::SenseKey;
use scsi_buffers::OwnedRequestBuffers;
use scsi_core::Request;
use scsi_core::ScsiResult;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

const EXPECT_PARAMETER_LIST_LENGTH: usize = size_of::<scsi::ProParameterList>();

fn make_pr_in_request(service_action: scsi::ServiceActionIn, allocation_length: usize) -> Request {
    let cdb = scsi::PersistentReserveIn {
        operation_code: scsi::ScsiOp::PERSISTENT_RESERVE_IN,
        service_action: scsi::PersistentReserveServiceActionIn::new()
            .with_service_action(service_action),
        reserved2: [0; 5],
        allocation_length: (allocation_length as u16).into(),
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..10].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

fn make_pr_in_request_with_service_action(service_action: u8, allocation_length: usize) -> Request {
    let cdb = scsi::PersistentReserveIn {
        operation_code: scsi::ScsiOp::PERSISTENT_RESERVE_IN,
        service_action: service_action.into(),
        reserved2: [0; 5],
        allocation_length: (allocation_length as u16).into(),
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..10].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

fn make_pr_out_request(
    service_action: scsi::ServiceActionOut,
    ty: scsi::ReservationType,
    scope: u8,
) -> Request {
    make_pr_out_request_with_parameter_list_length(
        service_action,
        ty,
        scope,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
}

fn make_scsi_result(
    scsi_status: ScsiStatus,
    srb_status: SrbStatus,
    sense_key: SenseKey,
    additional_sense_code: AdditionalSenseCode,
    additional_sense_code_qualifier: u8,
) -> ScsiResult {
    ScsiResult {
        scsi_status,
        srb_status,
        tx: 0,
        sense_data: Some(SenseData::new(
            sense_key,
            additional_sense_code,
            additional_sense_code_qualifier,
        )),
    }
}

fn make_scsi_result_no_sense(scsi_status: ScsiStatus, srb_status: SrbStatus) -> ScsiResult {
    ScsiResult {
        scsi_status,
        srb_status,
        tx: 0,
        sense_data: None,
    }
}

fn make_pr_out_request_with_parameter_list_length(
    service_action: scsi::ServiceActionOut,
    ty: scsi::ReservationType,
    scope: u8,
    parameter_list_length: usize,
) -> Request {
    let cdb = scsi::PersistentReserveOut {
        operation_code: scsi::ScsiOp::PERSISTENT_RESERVE_OUT,
        service_action: scsi::PersistentReserveServiceActionOut::new()
            .with_service_action(service_action),
        type_scope: scsi::PersistentReserveTypeScope::new()
            .with_reserve_type(ty)
            .with_scope(scope),
        reserved2: [0; 4],
        parameter_list_length: (parameter_list_length as u16).into(),
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..10].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

fn make_pr_out_request_with_reserve_type(
    service_action: scsi::ServiceActionOut,
    ty: u8,
) -> Request {
    let cdb = scsi::PersistentReserveOut {
        operation_code: scsi::ScsiOp::PERSISTENT_RESERVE_OUT,
        service_action: scsi::PersistentReserveServiceActionOut::new()
            .with_service_action(service_action),
        type_scope: ty.into(),
        reserved2: [0; 4],
        parameter_list_length: (EXPECT_PARAMETER_LIST_LENGTH as u16).into(),
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..10].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

fn make_pr_out_request_with_service_action(
    service_action: u8,
    ty: scsi::ReservationType,
    scope: u8,
) -> Request {
    let cdb = scsi::PersistentReserveOut {
        operation_code: scsi::ScsiOp::PERSISTENT_RESERVE_OUT,
        service_action: service_action.into(),
        type_scope: scsi::PersistentReserveTypeScope::new()
            .with_reserve_type(ty)
            .with_scope(scope),
        reserved2: [0; 4],
        parameter_list_length: (EXPECT_PARAMETER_LIST_LENGTH as u16).into(),
        control: 0,
    };
    let mut data = [0u8; 16];
    data[..10].copy_from_slice(cdb.as_bytes());
    Request {
        cdb: data,
        srb_flags: 0,
    }
}

fn check_pr_result(guest_mem: &GuestMemory, buff: &Vec<u8>) {
    check_guest_memory(guest_mem, 0, buff);
}

fn make_parameter_list_default_flags(
    reservation_key: u64,
    service_action_reservation_key: u64,
) -> scsi::ProParameterList {
    scsi::ProParameterList {
        reservation_key: reservation_key.into(),
        service_action_reservation_key: service_action_reservation_key.into(),
        obsolete: [0; 4],
        flags: scsi::ProParameterListFlags::new(),
        reserved3: 0,
        obsolete2: [0; 2],
    }
}

fn make_parameter_list_with_aptpl(
    reservation_key: u64,
    service_action_reservation_key: u64,
    aptpl: bool,
) -> scsi::ProParameterList {
    scsi::ProParameterList {
        reservation_key: reservation_key.into(),
        service_action_reservation_key: service_action_reservation_key.into(),
        obsolete: [0; 4],
        flags: scsi::ProParameterListFlags::new().with_aptpl(aptpl),
        reserved3: 0,
        obsolete2: [0; 2],
    }
}

fn make_parameter_list_with_sip(
    reservation_key: u64,
    service_action_reservation_key: u64,
    specify_initiator_ports: bool,
) -> scsi::ProParameterList {
    scsi::ProParameterList {
        reservation_key: reservation_key.into(),
        service_action_reservation_key: service_action_reservation_key.into(),
        obsolete: [0; 4],
        flags: scsi::ProParameterListFlags::new()
            .with_specify_initiator_ports(specify_initiator_ports),
        reserved3: 0,
        obsolete2: [0; 2],
    }
}

fn make_read_reservations_response(
    data: &mut [u8],
    generation: u32,
    ty: Option<scsi::ReservationType>,
    key: u64,
) {
    let header = scsi::PriReservationListHeader {
        generation: generation.into(),
        additional_length: if ty.is_some() {
            (size_of::<scsi::PriReservationDescriptor>() as u32).into()
        } else {
            0_u32.into()
        },
    };
    let mut temp_data = header.as_bytes().to_vec();
    let des;
    if let Some(ty) = ty {
        des = scsi::PriReservationDescriptor {
            type_scope: scsi::PersistentReserveTypeScope::new().with_reserve_type(ty),
            reservation_key: key.into(),
            ..FromZeros::new_zeroed()
        };
        temp_data.extend(des.as_bytes());
    }
    data[..temp_data.len()].copy_from_slice(temp_data.as_bytes());
}

fn make_read_keys_response(data: &mut [u8], generation: u32, key: u64) {
    let header = scsi::PriRegistrationListHeader {
        generation: generation.into(),
        additional_length: if key != 0 { 8_u32.into() } else { 0_u32.into() },
    };
    let mut temp_data = header.as_bytes().to_vec();
    if key != 0 {
        temp_data.extend(&key.to_be_bytes());
    }
    data[..temp_data.len()].copy_from_slice(temp_data.as_bytes());
}

fn make_read_full_status_response(
    data: &mut [u8],
    generation: u32,
    key: u64,
    hold_reservation: bool,
    ty: Option<scsi::ReservationType>,
) {
    let header = scsi::PriFullStatusListHeader {
        generation: generation.into(),
        additional_length: if key != 0 {
            ((size_of::<scsi::PriFullStatusDescriptorHeader>() + 8) as u32).into()
        } else {
            0_u32.into()
        },
    };
    let mut temp_data = header.as_bytes().to_vec();

    if key != 0 {
        let header = scsi::PriFullStatusDescriptorHeader {
            reservation_key: key.into(),
            flags: scsi::PriFullStatusDescriptorHeaderFlags::new()
                .with_all_target_ports(true)
                .with_reservation_holder(hold_reservation),
            type_scope: if let Some(reserve_type) = ty {
                scsi::PersistentReserveTypeScope::new().with_reserve_type(reserve_type)
            } else {
                scsi::PersistentReserveTypeScope::new()
            },
            relative_target_port_identifier: 0_u16.into(),
            additional_descriptor_length: 8_u32.into(),
            ..FromZeros::new_zeroed()
        };
        temp_data.extend(header.as_bytes());
        temp_data.extend(0_u64.as_bytes());
    }
    data[..temp_data.len()].copy_from_slice(temp_data.as_bytes());
}

fn make_report_capabilities_response(data: &mut [u8], aptpl: bool) {
    let caps = pr::ReservationCapabilities {
        write_exclusive: true,
        exclusive_access: true,
        write_exclusive_registrants_only: true,
        exclusive_access_registrants_only: true,
        write_exclusive_all_registrants: false,
        exclusive_access_all_registrants: false,
        persist_through_power_loss: true,
    };
    let persist_through_power_loss_active = if caps.persist_through_power_loss {
        aptpl
    } else {
        false
    };

    let flags = scsi::PriReportCapabilitiesFlags::new()
        .with_all_target_ports_capable(true)
        .with_persist_through_power_loss_capable(caps.persist_through_power_loss)
        .with_persist_through_power_loss_activated(persist_through_power_loss_active)
        .with_type_mask_valid(true);

    let type_mask = scsi::PriReportCapabilitiesTypeMask::new()
        .with_write_exclusive(caps.write_exclusive)
        .with_exclusive_access(caps.exclusive_access)
        .with_write_exclusive_registrants_only(caps.write_exclusive_registrants_only)
        .with_exclusive_access_registrants_only(caps.exclusive_access_registrants_only)
        .with_write_exclusive_all_registrants(caps.write_exclusive_all_registrants)
        .with_exclusive_access_all_registrants(caps.exclusive_access_all_registrants);

    let report = scsi::PriReportCapabilities {
        length: (size_of::<scsi::PriReportCapabilities>() as u16).into(),
        flags,
        type_mask,
        reserved7: [0; 2],
    };

    data[..].copy_from_slice(report.as_bytes());
}

async fn validate_pr_result(
    disk: &SimpleScsiDisk,
    generation: u32,
    key: u64,
    hold_reservation: bool,
    ty: Option<scsi::ReservationType>,
    aptpl: bool,
) {
    let mut read_reservations_response = vec![
        0u8;
        size_of::<scsi::PriReservationListHeader>()
            + size_of::<scsi::PriReservationDescriptor>()
    ];
    let mut read_keys_response = vec![0u8; size_of::<scsi::PriRegistrationListHeader>() + 8];
    let mut read_full_status_response = vec![
        0u8;
        size_of::<scsi::PriFullStatusListHeader>()
            + size_of::<scsi::PriFullStatusDescriptorHeader>()
            + 8
    ];
    let mut report_capabilities_response = vec![0u8; size_of::<scsi::PriReportCapabilities>()];
    let allocation_length = 4096;

    // read reservations
    let request = make_pr_in_request(scsi::ServiceActionIn::READ_RESERVATIONS, allocation_length);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    check_execute_scsi_pass(disk, &external_data.buffer(&guest_mem), &request).await;
    make_read_reservations_response(&mut read_reservations_response, generation, ty, key);
    check_pr_result(&guest_mem, &read_reservations_response);

    // read keys
    let request = make_pr_in_request(scsi::ServiceActionIn::READ_KEYS, allocation_length);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    check_execute_scsi_pass(disk, &external_data.buffer(&guest_mem), &request).await;
    make_read_keys_response(&mut read_keys_response, generation, key);
    check_pr_result(&guest_mem, &read_keys_response);

    // read full status
    let request = make_pr_in_request(scsi::ServiceActionIn::READ_FULL_STATUS, allocation_length);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    check_execute_scsi_pass(disk, &external_data.buffer(&guest_mem), &request).await;
    make_read_full_status_response(
        &mut read_full_status_response,
        generation,
        key,
        hold_reservation,
        ty,
    );
    check_pr_result(&guest_mem, &read_full_status_response);

    // report capabilities
    let request = make_pr_in_request(
        scsi::ServiceActionIn::REPORT_CAPABILITIES,
        allocation_length,
    );
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    check_execute_scsi_pass(disk, &external_data.buffer(&guest_mem), &request).await;
    make_report_capabilities_response(&mut report_capabilities_response, aptpl);
    check_pr_result(&guest_mem, &report_capabilities_response);
}

// "PersistentResrvation" scenario in scsi compliance tests.
async fn run_pr_scsi_compliance_with_reserve_type(ty: scsi::ReservationType) {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let key2 = 0x78563412;

    let scope = 0;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    // register ignore existing key
    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, scope);
    println!("1. register ignore existing key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    // reserve
    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, scope);
    println!("2. reserve use correct key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    // release
    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RELEASE, ty, scope);
    println!("3. release use correct key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    // register
    let data = make_parameter_list_default_flags(key1, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, scope);
    println!("4. register use correct key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;

    // register ignore existing key
    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, scope);
    println!("5. register ignore existing key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    // reserve
    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, scope);
    println!("6. reserve use correct key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    // clear
    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::CLEAR, ty, scope);
    println!("7. clear reserve use correct key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    // register ignore existing key
    let data = make_parameter_list_default_flags(key0, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, scope);
    println!("8. register preempt key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;

    // reserve
    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, scope);
    println!("9. reserve use preempt key expect pass ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;

    validate_pr_result(&disk, generation, key2, true, Some(ty), false).await;

    // Preempt
    let data = make_parameter_list_default_flags(key2, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT, ty, scope);
    println!("10. Preempt use preempty key expect passed ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;

    // Preempt Abort
    let data = make_parameter_list_default_flags(key2, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT_ABORT, ty, scope);
    println!("11. Preempt Abort use preempty key expect passed ...");
    check_execute_scsi_pass_with_tx(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        EXPECT_PARAMETER_LIST_LENGTH,
    )
    .await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;
}

#[async_test]
async fn validate_pr_scsi_compliance() {
    let supported_type = [
        scsi::ReservationType::WRITE_EXCLUSIVE,
        scsi::ReservationType::EXCLUSIVE,
        scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS,
        scsi::ReservationType::EXCLUSIVE_REGISTRANTS,
        scsi::ReservationType::WRITE_EXCLUSIVE_ALL_REGISTRANTS,
        scsi::ReservationType::EXCLUSIVE_ALL_REGISTRANTS,
    ];
    for ty in supported_type {
        println!("run scsi compliance test with reservation type {:?}", ty);
        run_pr_scsi_compliance_with_reserve_type(ty).await;
    }
}

async fn run_pr_invalid_reserve_type_scope(ty: u8) {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let key2 = 0x78563412;
    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_CDB,
        0,
    );

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request =
        make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty);
    println!("1 register ignore existing key use invalid type_scope expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::RESERVE, ty);
    println!("2 reserve use invalid type_scope expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    let data = make_parameter_list_default_flags(key1, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::REGISTER, ty);
    println!("3 register use invalid type_scope expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(
        scsi::ServiceActionOut::RESERVE,
        scsi::ReservationType::WRITE_EXCLUSIVE,
        0,
    );
    println!("4 reserve use supported type_scope expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;

    validate_pr_result(
        &disk,
        generation,
        key2,
        true,
        Some(scsi::ReservationType::WRITE_EXCLUSIVE),
        false,
    )
    .await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::RELEASE, ty);
    println!("5 release use invalid type_scope expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(
        &disk,
        generation,
        key2,
        true,
        Some(scsi::ReservationType::WRITE_EXCLUSIVE),
        false,
    )
    .await;

    // Preempt
    let data = make_parameter_list_default_flags(key2, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::PREEMPT, ty);
    println!("6 Preempt use invalid type_scope expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(
        &disk,
        generation,
        key2,
        true,
        Some(scsi::ReservationType::WRITE_EXCLUSIVE),
        false,
    )
    .await;

    // Preempt Abort
    let data = make_parameter_list_default_flags(key2, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::PREEMPT_ABORT, ty);
    println!("7 Preempt Abort use invalid type_scope expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(
        &disk,
        generation,
        key2,
        true,
        Some(scsi::ReservationType::WRITE_EXCLUSIVE),
        false,
    )
    .await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_reserve_type(scsi::ServiceActionOut::CLEAR, ty);
    println!("8 clear use invalid type_scope expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;
}

#[async_test]
async fn validate_pr_invalid_reserve_type_scope() {
    let invalid_ty = [
        0x00, 0x02, 0x04, 0x09, 0x0c, 0x0F, 0x11, 0x13, 0x15, 0x16, 0x17, 0x18, 0xF0, 0xF2, 0xF4,
        0xF5, 0xF6, 0xF7,
    ];
    for ty in invalid_ty {
        println!("validate invalid reserve type & scope {}", ty);
        run_pr_invalid_reserve_type_scope(ty).await;
    }
}

#[async_test]
async fn validate_pr_out_parameter_list_length() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;
    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::PARAMETER_LIST_LENGTH,
        0,
    );

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING,
        ty,
        0,
        EXPECT_PARAMETER_LIST_LENGTH - 1,
    );

    println!("1) parameter list length one byte too few expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING,
        ty,
        0,
        EXPECT_PARAMETER_LIST_LENGTH + 1,
    );
    println!("2) parameter list length one byte too many expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING,
        ty,
        0,
        0xfe76,
    );
    println!("3) magic parameter list length expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key0, false, None, false).await;
}

#[async_test]
async fn validate_pr_out_external_data_length() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;
    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_CDB,
        0,
    );

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH - 1, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, 0);
    println!("1) external_data one byte too few expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH + 1, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, 0);
    println!("2) external_data one byte too many expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;
}

#[async_test]
async fn validate_pr_out_sip() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;
    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_FIELD_PARAMETER_LIST,
        0,
    );

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_with_sip(key0, key1, true);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING,
        ty,
        0,
        EXPECT_PARAMETER_LIST_LENGTH,
    );

    println!("sip bit is not zero expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key0, false, None, false).await;
}

#[async_test]
async fn validate_pr_out_aptpl() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let key2 = 0x78563412;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_with_aptpl(key0, key1, true);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING,
        ty,
        0,
        EXPECT_PARAMETER_LIST_LENGTH,
    );

    println!("a. register ignore existing with aptpl bit is not zero expect passed ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, true).await;

    let data = make_parameter_list_with_aptpl(key1, key2, true);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER,
        ty,
        0,
        EXPECT_PARAMETER_LIST_LENGTH,
    );

    println!("b. register with aptpl bit is not zero expect passed ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, true).await;

    let data = make_parameter_list_with_aptpl(key0, key1, false);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request_with_parameter_list_length(
        scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING,
        ty,
        0,
        EXPECT_PARAMETER_LIST_LENGTH,
    );

    println!("c. register ignore existing with aptpl bit is zero expect passed ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;
}

#[async_test]
async fn validate_pr_out_invalid_service_action() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;
    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_CDB,
        0,
    );

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);

    println!("invalid service actions expect failed ...");
    for service_action in 7..31 {
        let request = make_pr_out_request_with_service_action(service_action, ty, 0);

        check_execute_scsi_failed_with_result(
            &disk,
            &external_data.buffer(&guest_mem),
            &request,
            &scsi_result,
        )
        .await;
    }

    validate_pr_result(&disk, generation, key0, false, None, false).await;
}

#[async_test]
async fn validate_pr_out_register() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let key2 = 0x78563412;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, 0);

    println!("1) register ignore existing with mismatch reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("2) register with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    let data = make_parameter_list_default_flags(key1, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, 0);

    println!("3) register ignore existing with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, 0);

    println!(
        "4) register ignore existing with empty service action reservation key expect pass ..."
    );
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("5) register with empty service action reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;
}

#[async_test]
async fn validate_pr_out_reserve_conflict() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let key2 = 0x78563412;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;
    let new_ty = scsi::ReservationType::WRITE_EXCLUSIVE;
    let scsi_result = make_scsi_result_no_sense(ScsiStatus::RESERVATION_CONFLICT, SrbStatus::ERROR);

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("1) register with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    let data = make_parameter_list_default_flags(key2, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("2) register with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, 0);

    println!("3) reserve with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, new_ty, 0);

    println!("4) register with different type expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("5) register with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key2, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("6) register with empty service action reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key2);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("7) register with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key2, false, None, false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, 0);

    println!("8) reserve with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;

    validate_pr_result(&disk, generation, key2, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("9) register with new service action reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, 0);

    println!("10) reserve again expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, 0);

    println!("11) reserve with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, new_ty, 0);

    println!("12) reserve with mismatch type expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RELEASE, ty, 0);

    println!("13) release with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::CLEAR, ty, 0);

    println!("14) clear with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT, ty, 0);

    println!("15) preempt with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key2, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT_ABORT, ty, 0);

    println!("16) preempt abort with mismatch reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;
}

#[async_test]
async fn validate_pr_out_mismatch_type() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_REGISTRANTS;
    let new_ty = scsi::ReservationType::WRITE_EXCLUSIVE;
    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_CDB,
        0,
    );

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER, ty, 0);

    println!("1) register with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, 0);

    println!("2) reserve with correct reservation key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RELEASE, new_ty, 0);

    println!("3) release with mismatch type expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT, new_ty, 0);

    println!("4) preempt with mismatch type expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT, ty, 0);

    println!("5) preempt with mismatch service action reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT_ABORT, new_ty, 0);

    println!("6) preempt abort with mismatch type expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::PREEMPT_ABORT, ty, 0);

    println!("7) preempt abort with mismatch service action reservation key expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    let data = make_parameter_list_default_flags(key1, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::CLEAR, new_ty, 0);

    println!("8) clear with mismatch type expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key0, false, None, false).await;
}

#[async_test]
async fn validate_pr_in_invalid_service_action() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_CDB,
        0,
    );
    let allocation_length = 4096;

    for service_action in 4..31 {
        let request = make_pr_in_request_with_service_action(service_action, allocation_length);
        let external_data = OwnedRequestBuffers::new(&[0]);
        let guest_mem = GuestMemory::allocate(allocation_length);
        check_execute_scsi_failed_with_result(
            &disk,
            &external_data.buffer(&guest_mem),
            &request,
            &scsi_result,
        )
        .await;
    }
}

#[async_test]
async fn validate_pr_in_data_buffer_too_small() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let scsi_result = make_scsi_result(
        ScsiStatus::CHECK_CONDITION,
        SrbStatus::INVALID_REQUEST,
        SenseKey::ILLEGAL_REQUEST,
        AdditionalSenseCode::INVALID_CDB,
        0,
    );
    let allocation_length = 4096;

    let request = make_pr_in_request(
        scsi::ServiceActionIn::READ_RESERVATIONS,
        allocation_length + 1,
    );
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("1) read reservations with smaller data buffer expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    let request = make_pr_in_request(scsi::ServiceActionIn::READ_KEYS, allocation_length + 1);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("2) read keys with smaller data buffer expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    let request = make_pr_in_request(
        scsi::ServiceActionIn::READ_FULL_STATUS,
        allocation_length + 1,
    );
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("3) read full status with smaller data buffer expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    let request = make_pr_in_request(
        scsi::ServiceActionIn::REPORT_CAPABILITIES,
        allocation_length + 1,
    );
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("4) report capabilities with smaller data buffer expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    // read reservations need at least size_of::<scsi::PriReservationListHeader>() data buffer
    let allocation_length = size_of::<scsi::PriReservationListHeader>() - 1;
    let request = make_pr_in_request(scsi::ServiceActionIn::READ_RESERVATIONS, allocation_length);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("5) read reservations with smaller allocation_length expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    // read keys need at least size_of::<scsi::PriRegistrationListHeader>() data buffer
    let allocation_length = size_of::<scsi::PriRegistrationListHeader>() - 1;
    let request = make_pr_in_request(scsi::ServiceActionIn::READ_KEYS, allocation_length);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("6) read keys with smaller allocation_length expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    // read full status need at least size_of::<scsi::PriFullStatusListHeader>() data buffer
    let allocation_length = size_of::<scsi::PriFullStatusListHeader>() - 1;
    let request = make_pr_in_request(scsi::ServiceActionIn::READ_FULL_STATUS, allocation_length);
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("7) read full status with smaller allocation_length expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;

    // report capabilities need at least size_of::<scsi::PriReportCapabilities>() data buffer
    let allocation_length = size_of::<scsi::PriReportCapabilities>() - 1;
    let request = make_pr_in_request(
        scsi::ServiceActionIn::REPORT_CAPABILITIES,
        allocation_length,
    );
    let external_data = OwnedRequestBuffers::new(&[0]);
    let guest_mem = GuestMemory::allocate(allocation_length);
    println!("8) report capabilities with smaller allocation_length expect failed ...");
    check_execute_scsi_failed_with_result(
        &disk,
        &external_data.buffer(&guest_mem),
        &request,
        &scsi_result,
    )
    .await;
}

async fn run_pr_in_truncates_results(
    disk: &SimpleScsiDisk,
    generation: u32,
    key: u64,
    hold_reservation: bool,
    ty: Option<scsi::ReservationType>,
) {
    let mut read_reservations_response = vec![
        0u8;
        size_of::<scsi::PriReservationListHeader>()
            + size_of::<scsi::PriReservationDescriptor>()
    ];
    let mut read_keys_response = vec![0u8; size_of::<scsi::PriRegistrationListHeader>() + 8];
    let mut read_full_status_response = vec![
        0u8;
        size_of::<scsi::PriFullStatusListHeader>()
            + size_of::<scsi::PriFullStatusDescriptorHeader>()
            + 8
    ];

    // read reservations
    let tx = if hold_reservation {
        read_reservations_response.len()
    } else {
        size_of::<scsi::PriReservationListHeader>()
    };
    for allocation_length in size_of::<scsi::PriReservationListHeader>()..tx {
        let request =
            make_pr_in_request(scsi::ServiceActionIn::READ_RESERVATIONS, allocation_length);
        let external_data = OwnedRequestBuffers::new(&[0]);
        let guest_mem = GuestMemory::allocate(allocation_length);
        println!(
            "read reservations truncated result with allocation_length {}",
            allocation_length
        );
        check_execute_scsi_pass_with_tx(
            disk,
            &external_data.buffer(&guest_mem),
            &request,
            allocation_length,
        )
        .await;
        make_read_reservations_response(&mut read_reservations_response, generation, ty, key);
        check_pr_result(
            &guest_mem,
            &read_reservations_response[..allocation_length].to_vec(),
        );
    }

    // read keys
    let tx = if key != 0 {
        read_keys_response.len()
    } else {
        size_of::<scsi::PriRegistrationListHeader>()
    };
    for allocation_length in size_of::<scsi::PriRegistrationListHeader>()..tx {
        let request = make_pr_in_request(scsi::ServiceActionIn::READ_KEYS, allocation_length);
        let external_data = OwnedRequestBuffers::new(&[0]);
        let guest_mem = GuestMemory::allocate(allocation_length);
        println!(
            "read keys truncated result with allocation_length {}",
            allocation_length
        );
        check_execute_scsi_pass_with_tx(
            disk,
            &external_data.buffer(&guest_mem),
            &request,
            allocation_length,
        )
        .await;
        make_read_keys_response(&mut read_keys_response, generation, key);
        check_pr_result(
            &guest_mem,
            &read_keys_response[..allocation_length].to_vec(),
        );
    }

    // read full status
    let tx = if key != 0 {
        read_full_status_response.len()
    } else {
        size_of::<scsi::PriFullStatusListHeader>()
    };
    for allocation_length in size_of::<scsi::PriFullStatusListHeader>()..tx {
        let request =
            make_pr_in_request(scsi::ServiceActionIn::READ_FULL_STATUS, allocation_length);
        let external_data = OwnedRequestBuffers::new(&[0]);
        let guest_mem = GuestMemory::allocate(allocation_length);
        println!(
            "read full status truncated result with allocation_length {}",
            allocation_length
        );
        check_execute_scsi_pass_with_tx(
            disk,
            &external_data.buffer(&guest_mem),
            &request,
            allocation_length,
        )
        .await;
        make_read_full_status_response(
            &mut read_full_status_response,
            generation,
            key,
            hold_reservation,
            ty,
        );
        check_pr_result(
            &guest_mem,
            &read_full_status_response[..allocation_length].to_vec(),
        );
    }
}

#[async_test]
async fn validate_pr_in_truncates_results() {
    let (disk, _state) = new_scsi_disk(512, 4096, 1024, false, true, true);

    let mut generation = 0;
    let key0 = 0;
    let key1 = 0x12345678;

    let scope = 0;
    let ty = scsi::ReservationType::WRITE_EXCLUSIVE_ALL_REGISTRANTS;

    validate_pr_result(&disk, generation, key0, false, None, false).await;

    let data = make_parameter_list_default_flags(key0, key1);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::REGISTER_IGNORE_EXISTING, ty, scope);
    println!("a1. register ignore existing key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;
    generation += 1;

    validate_pr_result(&disk, generation, key1, false, None, false).await;

    println!("a2. validate truncates results with register key without reservation");
    run_pr_in_truncates_results(&disk, generation, key1, false, None).await;

    let data = make_parameter_list_default_flags(key1, key0);
    let guest_mem = make_guest_memory(data.as_bytes());
    let external_data = OwnedRequestBuffers::linear(0, EXPECT_PARAMETER_LIST_LENGTH, true);
    let request = make_pr_out_request(scsi::ServiceActionOut::RESERVE, ty, scope);
    println!("b1. reserve use correct key expect pass ...");
    check_execute_scsi_pass(&disk, &external_data.buffer(&guest_mem), &request).await;

    validate_pr_result(&disk, generation, key1, true, Some(ty), false).await;

    println!("b2. validate truncates results with register key with reservation");
    run_pr_in_truncates_results(&disk, generation, key1, true, Some(ty)).await;
}
