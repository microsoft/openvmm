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
use scsi_defs::Cdb12;
use scsi_defs::Cdb16;
use scsi_defs::Cdb6ReadWrite;
use scsi_defs::CdbInquiry;
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
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Various fuzzer actions for storvsp. At the most generic, we expect
/// `SendRawPacket` to be the most basic primitive: this will send a vmbus
/// packet that is a series of bytes. This, combined with `ReadCompletion`
/// and `AttachDisk`/`DetachDisk` should be enough to exercise most of the
/// storvsp code paths that we care about. That being said, we can also
/// improve the efficiency of the search by making the packets more well
/// formed in two ways:
/// 1. `SendNiceReadWritePacket` will send a read or write packet, with
///   a reasonable CDB, SRB, etc. This is likely to get to the common
///   read / write paths.
/// 2. `SendTargetScsiOp` will send an arbitrary SRB with the correct
///   CDB length for some SCSI operations that are particularly handled
///   in storvsp. For example, we should quickly see `REPORT LUNS` and
///   `INQUIRY` scsi ops, where many hours of running without this special
///   case didn't cover those paths.
#[derive(Arbitrary)]
enum StorvspFuzzAction {
    SendNiceReadWritePacket,
    SendTargetScsiOp(TargetScsiOp),
    SendRawPacket(FuzzOutgoingPacketType),
    ReadCompletion,
    AttachDisk(ScsiPath),
    DetachDisk(ScsiPath),
}

/// What type of VMBUS packet to put in the ring;
/// special case `GpaDirectPacket` because that needs
/// different handling to reference GPAs.
#[derive(Arbitrary)]
enum FuzzOutgoingPacketType {
    AnyOutgoingPacket,
    GpaDirectPacket,
}

/// Key SCSI operations that storvsp would handle.
#[derive(Arbitrary)]
enum TargetScsiOp {
    Inquiry,
    ReportLuns,
    ReadWrite6,
    ReadWrite10,
    ReadWrite12,
    ReadWrite16,
}

/// Creates an SRB with a CDB that is reasonably well formed for
/// the given `TargetScsiOp`.
fn create_targeted_scsi_packet(
    u: &mut Unstructured<'_>,
    op: TargetScsiOp,
) -> Result<protocol::ScsiRequest, arbitrary::Error> {
    let mut scsi_req: protocol::ScsiRequest = u.arbitrary()?;

    match op {
        TargetScsiOp::Inquiry => {
            let mut bytes = vec![0; size_of::<CdbInquiry>()];
            u.fill_buffer(&mut bytes)?;

            let cdb = CdbInquiry::mut_from_bytes(bytes.as_mut_slice())
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;

            cdb.operation_code = ScsiOp::INQUIRY;

            scsi_req.payload[0..(bytes.len())].copy_from_slice(bytes.as_slice());
        }
        TargetScsiOp::ReportLuns => {
            let mut bytes = vec![0; size_of::<Cdb10>()];
            u.fill_buffer(&mut bytes)?;

            let cdb = Cdb10::mut_from_bytes(bytes.as_mut_slice())
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;
            cdb.operation_code = ScsiOp::REPORT_LUNS;

            scsi_req.payload[0..(bytes.len())].copy_from_slice(bytes.as_slice());
        }
        TargetScsiOp::ReadWrite6 => {
            let mut bytes = vec![0; size_of::<Cdb6ReadWrite>()];
            u.fill_buffer(&mut bytes)?;

            let cdb = Cdb6ReadWrite::mut_from_bytes(bytes.as_mut_slice())
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;

            cdb.operation_code = ScsiOp::READ;
            cdb.transfer_blocks = (arbitrary_byte_len(u)? / 512) as u8;

            scsi_req.payload[0..(bytes.len())].copy_from_slice(bytes.as_slice());
        }
        TargetScsiOp::ReadWrite10 => {
            let mut bytes = vec![0; size_of::<Cdb10>()];
            u.fill_buffer(&mut bytes)?;

            let cdb = Cdb10::mut_from_bytes(bytes.as_mut_slice())
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;

            cdb.operation_code = ScsiOp::READ;
            cdb.transfer_blocks = ((arbitrary_byte_len(u)? / 512) as u16).into();

            scsi_req.payload[0..(bytes.len())].copy_from_slice(bytes.as_slice());
        }
        TargetScsiOp::ReadWrite12 => {
            let mut bytes = vec![0; size_of::<Cdb12>()];
            u.fill_buffer(&mut bytes)?;

            let cdb = Cdb12::mut_from_bytes(bytes.as_mut_slice())
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;

            cdb.operation_code = ScsiOp::READ;
            cdb.transfer_blocks = ((arbitrary_byte_len(u)? / 512) as u32).into();

            scsi_req.payload[0..(bytes.len())].copy_from_slice(bytes.as_slice());
        }
        TargetScsiOp::ReadWrite16 => {
            let mut bytes = vec![0; size_of::<Cdb16>()];
            u.fill_buffer(&mut bytes)?;

            let cdb = Cdb16::mut_from_bytes(bytes.as_mut_slice())
                .map_err(|_| arbitrary::Error::IncorrectFormat)?;

            cdb.operation_code = ScsiOp::READ;
            cdb.transfer_blocks = ((arbitrary_byte_len(u)? / 512) as u32).into();

            scsi_req.payload[0..(bytes.len())].copy_from_slice(bytes.as_slice());
        }
    };

    Ok(scsi_req)
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
        data_in: if cdb.operation_code == ScsiOp::READ {
            1
        } else {
            0
        },
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

/// Sometimes, replace the completely valid packet with an arbitrary one.
fn swizzle_packet(
    u: &mut Unstructured<'_>,
    packet: protocol::Packet,
) -> Result<protocol::Packet, arbitrary::Error> {
    if u.ratio(9, 10)? {
        Ok(packet)
    } else {
        u.arbitrary()
    }
}

/// Main fuzzing loop. Separate out into an async function so that the top-level
/// `do_fuzz` routine can wait for either this to complete or for the test worker
/// task to complete.
///
/// This function will run until the Unstructured is exhausted, but can get stuck
/// if the ring is full or if the storvsp worker terminates because of some sort
/// of guest -> host packet corruption.
async fn do_fuzz_loop(
    u: &mut Unstructured<'_>,
    guest: &mut TestGuest,
    controller: &ScsiController,
) -> Result<(), anyhow::Error> {
    if u.ratio(9, 10)? {
        let negotiate_packet = swizzle_packet(
            u,
            protocol::Packet {
                operation: protocol::Operation::BEGIN_INITIALIZATION,
                flags: 0,
                status: protocol::NtStatus::SUCCESS,
            },
        )?;
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest.queue.split().0.read().await?;

        let version_packet = swizzle_packet(
            u,
            protocol::Packet {
                operation: protocol::Operation::QUERY_PROTOCOL_VERSION,
                flags: 0,
                status: protocol::NtStatus::SUCCESS,
            },
        )?;
        let version = if u.ratio(9, 10)? {
            protocol::ProtocolVersion {
                major_minor: protocol::VERSION_BLUE,
                reserved: 0,
            }
        } else {
            u.arbitrary()?
        };
        guest
            .send_data_packet_sync(&[version_packet.as_bytes(), version.as_bytes()])
            .await;
        guest.queue.split().0.read().await?;

        let properties_packet = swizzle_packet(
            u,
            protocol::Packet {
                operation: protocol::Operation::QUERY_PROPERTIES,
                flags: 0,
                status: protocol::NtStatus::SUCCESS,
            },
        )?;
        guest
            .send_data_packet_sync(&[properties_packet.as_bytes()])
            .await;
        guest.queue.split().0.read().await?;

        let negotiate_packet = swizzle_packet(
            u,
            protocol::Packet {
                operation: protocol::Operation::END_INITIALIZATION,
                flags: 0,
                status: protocol::NtStatus::SUCCESS,
            },
        )?;
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest.queue.split().0.read().await?;
    }

    while !u.is_empty() {
        let action = u.arbitrary::<StorvspFuzzAction>()?;
        match action {
            StorvspFuzzAction::SendNiceReadWritePacket => {
                send_arbitrary_readwrite_packet(u, guest).await?;
            }
            StorvspFuzzAction::SendRawPacket(packet_type) => match packet_type {
                FuzzOutgoingPacketType::AnyOutgoingPacket => {
                    let packet_types = [
                        OutgoingPacketType::InBandNoCompletion,
                        OutgoingPacketType::InBandWithCompletion,
                        OutgoingPacketType::Completion,
                    ];

                    let payload: Vec<Vec<u8>> = u.arbitrary()?;
                    let payload_vec = payload.iter().map(|x| x.as_slice()).collect::<Vec<&[u8]>>();
                    let payload_slice = payload_vec.as_slice();

                    let packet = OutgoingPacket {
                        transaction_id: u.arbitrary()?,
                        packet_type: *u.choose(&packet_types)?,
                        payload: payload_slice,
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
            StorvspFuzzAction::AttachDisk(path) => {
                let disk_len_sectors = u.int_in_range(1..=8192)?; // up to 4mb in 512 byte sectors
                let disk = scsidisk::SimpleScsiDisk::new(
                    disklayer_ram::ram_disk(disk_len_sectors * 512, false).unwrap(),
                    Default::default(),
                );

                let _ = controller.attach(path, ScsiControllerDisk::new(Arc::new(disk)));
            }
            StorvspFuzzAction::DetachDisk(path) => {
                let _ = controller.remove(path);
            }
            StorvspFuzzAction::SendTargetScsiOp(op) => {
                let packet = protocol::Packet {
                    operation: protocol::Operation::EXECUTE_SRB,
                    flags: 0,
                    status: protocol::NtStatus::SUCCESS,
                };

                let scsi_req = create_targeted_scsi_packet(u, op)?;
                send_gpa_direct_packet(
                    guest,
                    &[packet.as_bytes(), scsi_req.as_bytes()],
                    u.arbitrary()?,
                    arbitrary_byte_len(u)?,
                    u.arbitrary()?,
                )
                .await?;
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

        let controller = Arc::new(ScsiController::new());

        // Most of the time, start up with one attached disk.
        if u.ratio(9, 10)? {
            let disk_len_sectors = u.int_in_range(1..=262144)?; // up to 128mb in 512 byte sectors
            let disk = scsidisk::SimpleScsiDisk::new(
                disklayer_ram::ram_disk(disk_len_sectors * 512, false).unwrap(),
                Default::default(),
            );

            controller.attach(u.arbitrary()?, ScsiControllerDisk::new(Arc::new(disk)))?;
        }

        // storvsp's worker will try to allocate a Slab with `io_queue_depth` entries
        // of `ScsiRequestState`, each of which is essentially 2 x `u64`.
        let io_queue_depth: Option<u32> = Some((u.arbitrary_len::<u64>()? / 2) as u32);
        let test_worker = TestWorker::start(
            &controller,
            driver.clone(),
            test_guest_mem.clone(),
            host,
            io_queue_depth,
        );

        let mut guest = TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        let mut fuzz_loop = pin!(do_fuzz_loop(u, &mut guest, &controller).fuse());
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
