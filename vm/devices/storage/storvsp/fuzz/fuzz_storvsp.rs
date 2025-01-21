// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use futures::select;
use futures::FutureExt;
use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use pal_async::DefaultPool;
use scsi_defs::Cdb10;
use scsi_defs::ScsiOp;
use std::pin::pin;
use std::sync::Arc;
use storvsp::protocol;
use storvsp::test_helpers::TestGuest;
use storvsp::test_helpers::TestWorker;
use storvsp::ScsiController;
use storvsp::ScsiControllerDisk;
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
    SendReadWritePacket,
    SendRawPacket(FuzzOutgoingPacketType),
    ReadCompletion,
}

#[derive(Arbitrary)]
enum FuzzOutgoingPacketType {
    AnyOutgoingPacket,
    GpaDirectPacket,
}

/// Return an arbitrary byte length that can be sent in a GPA direct
/// packet. The byte length is limited to the maximum number of pages
/// that could fit into a `PagedRange` (at least with how we store the
/// list of pages in the fuzzer ...).
fn arbitrary_byte_len(u: &mut Unstructured<'_>) -> Result<usize, arbitrary::Error> {
    let max_byte_len = u.arbitrary_len::<u64>()? * PAGE_SIZE;
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
    let end_page = start_page
        .checked_add(byte_len.try_into()?)
        .map(|v| v.div_ceil(PAGE_SIZE as u64))
        .ok_or(arbitrary::Error::IncorrectFormat)?;

    let gpns: Vec<u64> = (start_page..end_page).collect();
    let pages = PagedRange::new(gpa_start as usize % PAGE_SIZE, byte_len, gpns.as_slice())
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

/// Send a reasonably well structured read or write packet to storvsp.
/// While the fuzzer should eventually discover these paths by poking at
/// arbitrary GpaDirect packet payload, make the search more efficient by
/// generating a packet that is more likely to pass basic parsing checks.
async fn send_arbitrary_readwrite_packet(
    u: &mut Unstructured<'_>,
    guest: &mut TestGuest,
) -> Result<(), anyhow::Error> {
    let path: ScsiPath = u.arbitrary()?;
    let gpa = u.arbitrary::<u64>()?;
    let byte_len = arbitrary_byte_len(u)?;

    let block: u32 = u.arbitrary()?;
    let transaction_id: u64 = u.arbitrary()?;

    let packet = protocol::Packet {
        operation: protocol::Operation::EXECUTE_SRB,
        flags: 0,
        status: protocol::NtStatus::SUCCESS,
    };

    // TODO: read6, read12, read16, write6, write12, write16, etc. (READ is read10, WRITE is write10)
    let scsiop_choices = [ScsiOp::READ, ScsiOp::WRITE];
    let cdb = Cdb10 {
        operation_code: *(u.choose(&scsiop_choices)?),
        logical_block: block.into(),
        transfer_blocks: ((byte_len / 512) as u16).into(),
        ..FromZeros::new_zeroed()
    };

    let mut scsi_req = protocol::ScsiRequest {
        target_id: path.target,
        path_id: path.path,
        lun: path.lun,
        length: protocol::SCSI_REQUEST_LEN_V2 as u16,
        cdb_length: size_of::<Cdb10>() as u8,
        data_transfer_length: byte_len.try_into()?,
        data_in: 1,
        ..FromZeros::new_zeroed()
    };

    scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());

    send_gpa_direct_packet(
        guest,
        &[packet.as_bytes(), scsi_req.as_bytes()],
        gpa,
        byte_len,
        transaction_id,
    )
    .await
}

async fn do_fuzz_loop(
    u: &mut Unstructured<'_>,
    guest: &mut TestGuest,
) -> Result<(), anyhow::Error> {
    if u.ratio(9, 10)? {
        // TODO: [use-arbitrary-input] (e.g., munge the negotiation packets)
        guest.perform_protocol_negotiation().await;
    }

    while !u.is_empty() {
        let action = u.arbitrary::<StorvspFuzzAction>()?;
        match action {
            StorvspFuzzAction::SendReadWritePacket => {
                send_arbitrary_readwrite_packet(u, guest).await?;
            }
            StorvspFuzzAction::SendRawPacket(packet_type) => {
                match packet_type {
                    FuzzOutgoingPacketType::AnyOutgoingPacket => {
                        let packet_types = [
                            OutgoingPacketType::InBandNoCompletion,
                            OutgoingPacketType::InBandWithCompletion,
                            OutgoingPacketType::Completion,
                        ];
                        let payload = u.arbitrary::<protocol::Packet>()?;
                        // TODO: [use-arbitrary-input] (send a byte blob of arbitrary length rather
                        // than a fixed-size arbitrary packet)
                        let packet = OutgoingPacket {
                            transaction_id: u.arbitrary()?,
                            packet_type: *u.choose(&packet_types)?,
                            payload: &[payload.as_bytes()], // TODO: [use-arbitrary-input]
                        };

                        guest.queue.split().1.write(packet).await?;
                    }
                    FuzzOutgoingPacketType::GpaDirectPacket => {
                        let header = u.arbitrary::<protocol::Packet>()?;
                        let scsi_req = u.arbitrary::<protocol::ScsiRequest>()?;

                        send_gpa_direct_packet(
                            guest,
                            &[header.as_bytes(), scsi_req.as_bytes()],
                            u.arbitrary()?,
                            arbitrary_byte_len(u)?,
                            u.arbitrary()?,
                        )
                        .await?
                    }
                }
            }
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
    DefaultPool::run_with(|driver| async move {
        let (host, guest_channel) = connected_async_channels(16 * 1024); // TODO: [use-arbitrary-input]
        let guest_queue = Queue::new(guest_channel).unwrap();

        let test_guest_mem = GuestMemory::allocate(u.int_in_range(1..=256)? * PAGE_SIZE);
        let controller = ScsiController::new();
        let disk_len_sectors = u.int_in_range(1..=1048576)?; // up to 512mb in 512 byte sectors
        let disk = scsidisk::SimpleScsiDisk::new(
            disklayer_ram::ram_disk(disk_len_sectors * 512, false).unwrap(),
            Default::default(),
        );
        controller.attach(u.arbitrary()?, ScsiControllerDisk::new(Arc::new(disk)))?;

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

    // Always keep the corpus, since errors are a reasonable outcome.
    // A future optimization would be to reject any corpus entries that
    // result in the inability to generate arbitrary data from the Unstructured...
});
