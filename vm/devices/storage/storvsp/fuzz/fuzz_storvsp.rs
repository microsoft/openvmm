// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use futures::FutureExt;
use futures::select;
use guestmem::GuestMemory;
use guestmem::ranges::PagedRange;
use pal_async::DefaultPool;
use scsi_defs::Cdb10;
use scsi_defs::CdbInquiry;
use scsi_defs::ScsiOp;
use std::pin::pin;
use std::sync::Arc;
use storvsp::ScsiController;
use storvsp::ScsiControllerDisk;
use storvsp::test_helpers::TestGuest;
use storvsp::test_helpers::TestWorker;
use storvsp_resources::ScsiPath;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::connected_async_channels;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::PAGE_SIZE;
use xtask_fuzz::fuzz_target;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

#[derive(Arbitrary)]
enum StorvspFuzzAction {
    SendScsiPacket(FuzzCdbType),
    SendRawPacket(FuzzOutgoingPacketType),
    SendResetPacket(FuzzResetType),
    ReadCompletion,
}

#[derive(Arbitrary)]
enum FuzzCdbType {
    ReadWrite,
    ReportLuns,
    Inquiry,
}

#[derive(Arbitrary)]
enum FuzzResetType {
    Bus,
    Adapter,
    Lun,
}

#[derive(Arbitrary)]
enum FuzzOutgoingPacketType {
    AnyOutgoingPacket,
    GpaDirectPacket,
}

/// Maximum number of pages in a single GPA direct packet. Kept small to
/// avoid per-packet Vec<u64> heap allocations — the GPN array is stack-allocated.
const MAX_GPA_PAGES: usize = 8;

/// Return an arbitrary byte length for GPA direct packets, capped to
/// MAX_GPA_PAGES pages to keep allocations bounded.
fn arbitrary_byte_len(u: &mut Unstructured<'_>) -> Result<usize, arbitrary::Error> {
    let max_byte_len = MAX_GPA_PAGES * PAGE_SIZE;
    u.int_in_range(0..=max_byte_len)
}

/// Sends a GPA direct packet (a type of vmbus packet that references guest memory,
/// the typical packet type used for SCSI requests) to storvsp.
async fn send_gpa_direct_packet(
    guest: &mut TestGuest,
    payload: &[&[u8]],
    gpa_start: u64,
    byte_len: usize,
    transaction_id: u64,
) -> Result<(), anyhow::Error> {
    let start_page: u64 = gpa_start / PAGE_SIZE as u64;
    let page_offset = gpa_start as usize % PAGE_SIZE;
    // Clamp byte_len so the GPA range fits in MAX_GPA_PAGES.
    let byte_len = byte_len.min(MAX_GPA_PAGES * PAGE_SIZE - page_offset);

    let end_addr = gpa_start
        .checked_add(byte_len as u64)
        .ok_or(arbitrary::Error::IncorrectFormat)?;
    let end_page = end_addr.div_ceil(PAGE_SIZE as u64);
    let page_count = (end_page - start_page) as usize;
    if page_count > MAX_GPA_PAGES {
        return Err(arbitrary::Error::IncorrectFormat.into());
    }

    let mut gpns = [0u64; MAX_GPA_PAGES];
    for (i, gpn) in (start_page..end_page).enumerate() {
        gpns[i] = gpn;
    }
    let pages = PagedRange::new(page_offset, byte_len, &gpns[..page_count])
        .ok_or(arbitrary::Error::IncorrectFormat)?;

    guest
        .queue
        .split()
        .1
        .write(OutgoingPacket {
            packet_type: OutgoingPacketType::GpaDirect(&[pages]),
            transaction_id,
            payload,
        })
        .await
        .map_err(|e| e.into())
}

/// Build and send an EXECUTE_SRB packet with the given CDB type.
/// The three SCSI send paths (read/write, report-luns, inquiry) share
/// identical packet construction — only the CDB content differs.
async fn send_scsi_packet(
    u: &mut Unstructured<'_>,
    guest: &mut TestGuest,
    cdb_type: &FuzzCdbType,
) -> Result<(), anyhow::Error> {
    let path: ScsiPath = u.arbitrary()?;
    let gpa = u.arbitrary::<u64>()?;
    let byte_len = arbitrary_byte_len(u)?;
    let transaction_id: u64 = u.arbitrary()?;

    let packet = storvsp_protocol::Packet {
        operation: storvsp_protocol::Operation::EXECUTE_SRB,
        flags: 0,
        status: storvsp_protocol::NtStatus::SUCCESS,
    };

    let mut cdb_buf = [0u8; 16];
    let (cdb_len, is_read) = match cdb_type {
        FuzzCdbType::ReadWrite => {
            let block: u32 = u.arbitrary()?;
            let ops = [ScsiOp::READ, ScsiOp::WRITE];
            let op = *u.choose(&ops)?;
            let cdb = Cdb10 {
                operation_code: op,
                logical_block: block.into(),
                transfer_blocks: ((byte_len / 512) as u16).into(),
                ..FromZeros::new_zeroed()
            };
            cdb_buf[..10].copy_from_slice(cdb.as_bytes());
            (size_of::<Cdb10>(), op == ScsiOp::READ)
        }
        FuzzCdbType::ReportLuns => {
            // REPORT_LUNS is a 12-byte CDB. Only the opcode byte matters
            // for storvsp dispatch, but set the length correctly.
            cdb_buf[0] = ScsiOp::REPORT_LUNS.0;
            (12, true)
        }
        FuzzCdbType::Inquiry => {
            let cdb = CdbInquiry {
                operation_code: ScsiOp::INQUIRY.0,
                page_code: u.arbitrary()?,
                allocation_length: (byte_len as u16).into(),
                ..FromZeros::new_zeroed()
            };
            let bytes = cdb.as_bytes();
            cdb_buf[..bytes.len()].copy_from_slice(bytes);
            (bytes.len(), true)
        }
    };

    let mut scsi_req = storvsp_protocol::ScsiRequest {
        target_id: path.target,
        path_id: path.path,
        lun: path.lun,
        length: storvsp_protocol::SCSI_REQUEST_LEN_V2 as u16,
        cdb_length: cdb_len as u8,
        data_transfer_length: byte_len.try_into()?,
        data_in: if is_read { 1 } else { 0 },
        ..FromZeros::new_zeroed()
    };

    scsi_req.payload[..cdb_len].copy_from_slice(&cdb_buf[..cdb_len]);

    send_gpa_direct_packet(
        guest,
        &[packet.as_bytes(), scsi_req.as_bytes()],
        gpa,
        byte_len,
        transaction_id,
    )
    .await
}

/// Send a reset packet (RESET_BUS, RESET_ADAPTER, or RESET_LUN).
async fn send_reset_packet(
    u: &mut Unstructured<'_>,
    guest: &mut TestGuest,
    reset_type: &FuzzResetType,
) -> Result<(), anyhow::Error> {
    let operation = match reset_type {
        FuzzResetType::Bus => storvsp_protocol::Operation::RESET_BUS,
        FuzzResetType::Adapter => storvsp_protocol::Operation::RESET_ADAPTER,
        FuzzResetType::Lun => storvsp_protocol::Operation::RESET_LUN,
    };

    let packet = storvsp_protocol::Packet {
        operation,
        flags: 0,
        status: storvsp_protocol::NtStatus::SUCCESS,
    };

    let transaction_id: u64 = u.arbitrary()?;

    guest
        .queue
        .split()
        .1
        .write(OutgoingPacket {
            packet_type: OutgoingPacketType::InBandWithCompletion,
            transaction_id,
            payload: &[packet.as_bytes()],
        })
        .await?;

    Ok(())
}

async fn do_fuzz_loop(
    u: &mut Unstructured<'_>,
    guest: &mut TestGuest,
) -> Result<(), anyhow::Error> {
    // Always negotiate — without init, storvsp just rejects all packets
    // with INVALID_DEVICE_STATE. The SendRawPacket paths can still exercise
    // the init state machine through mutation if needed.
    guest.perform_protocol_negotiation().await;

    while !u.is_empty() {
        let action = u.arbitrary::<StorvspFuzzAction>()?;
        match action {
            StorvspFuzzAction::SendScsiPacket(cdb_type) => {
                send_scsi_packet(u, guest, &cdb_type).await?;
            }
            StorvspFuzzAction::SendResetPacket(reset_type) => {
                send_reset_packet(u, guest, &reset_type).await?;
            }
            StorvspFuzzAction::SendRawPacket(packet_type) => match packet_type {
                FuzzOutgoingPacketType::AnyOutgoingPacket => {
                    let packet_types = [
                        OutgoingPacketType::InBandNoCompletion,
                        OutgoingPacketType::InBandWithCompletion,
                        OutgoingPacketType::Completion,
                    ];
                    let payload_len = u.int_in_range(0..=64)?;
                    let payload: &[u8] = u.bytes(payload_len)?;
                    let packet = OutgoingPacket {
                        transaction_id: u.arbitrary()?,
                        packet_type: *u.choose(&packet_types)?,
                        payload: &[payload],
                    };

                    guest.queue.split().1.write(packet).await?;
                }
                FuzzOutgoingPacketType::GpaDirectPacket => {
                    let header = u.arbitrary::<storvsp_protocol::Packet>()?;
                    let scsi_req = u.arbitrary::<storvsp_protocol::ScsiRequest>()?;

                    send_gpa_direct_packet(
                        guest,
                        &[header.as_bytes(), scsi_req.as_bytes()],
                        u.arbitrary()?,
                        arbitrary_byte_len(u)?,
                        u.arbitrary()?,
                    )
                    .await?
                }
            },
            StorvspFuzzAction::ReadCompletion => {
                // Read completion(s) from the storvsp -> guest queue. This shouldn't
                // evoke any specific storvsp behavior, but is important to eventually
                // allow forward progress of various code paths.
                //
                // Ignore the result, since vmbus returns error if the queue is empty,
                // but that's fine for the fuzzer ...
                let _ = guest.queue.split().0.try_read();
            }
        }
    }

    Ok(())
}

fn do_fuzz(u: &mut Unstructured<'_>) -> Result<(), anyhow::Error> {
    DefaultPool::run_with(async |driver| {
        let (host, guest_channel) = connected_async_channels(4 * 1024);
        let guest_queue = Queue::new(guest_channel).unwrap();

        let test_guest_mem = GuestMemory::allocate(4 * PAGE_SIZE);
        let controller = ScsiController::new();
        // Attach 0-3 disks at arbitrary paths. 0 disks exercises the
        // empty-controller code paths (REPORT_LUNS returns empty list,
        // INQUIRY returns "not present", other ops return INVALID_LUN).
        let num_disks = u.int_in_range(0..=3)?;
        for _ in 0..num_disks {
            let disk = scsidisk::SimpleScsiDisk::new(
                disklayer_ram::ram_disk(64 * 512, false).unwrap(),
                Default::default(),
            );
            // Ignore errors (e.g., duplicate paths) — fewer disks is fine.
            let _ = controller.attach(u.arbitrary()?, ScsiControllerDisk::new(Arc::new(disk)));
        }

        let test_worker = TestWorker::start(
            controller,
            driver.clone(),
            test_guest_mem.clone(),
            host,
            None,
        );

        let mut guest = TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        let mut fuzz_loop = pin!(do_fuzz_loop(u, &mut guest).fuse());
        let mut teardown = pin!(test_worker.teardown_ignore().fuse());

        select! {
            _r1 = fuzz_loop => xtask_fuzz::fuzz_eprintln!("test case exhausted arbitrary data"),
            _r2 = teardown => xtask_fuzz::fuzz_eprintln!("test worker completed"),
        }

        Ok::<(), anyhow::Error>(())
    })?;

    Ok::<(), anyhow::Error>(())
}

fuzz_target!(|input: &[u8]| {
    xtask_fuzz::init_tracing_if_repro();
    let _ = do_fuzz(&mut Unstructured::new(input));
});
