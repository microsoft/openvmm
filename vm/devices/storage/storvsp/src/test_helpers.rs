// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! StorVSP test helpers.
//!
//! These are used both by unit tests and by benchmarks.

// Benchmarks do not use all the code here, but unit tests should.
#![cfg_attr(not(test), allow(dead_code))]

use super::protocol;
use crate::InitState;
use crate::PacketError;
use crate::Protocol;
use crate::ProtocolState;
use crate::ScsiController;
use crate::ScsiPath;
use crate::Worker;
use crate::WorkerError;
use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use guestmem::MemoryRead;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::RwLock;
use scsi::srb::SrbStatus;
use scsi::ScsiOp;
use scsi_defs as scsi;
use std::sync::Arc;
use vmbus_async::queue::IncomingPacket;
use vmbus_async::queue::OutgoingPacket;
use vmbus_async::queue::Queue;
use vmbus_channel::RawAsyncChannel;
use vmbus_ring as ring;
use vmbus_ring::FlatRingMem;
use vmbus_ring::OutgoingPacketType;
use vmbus_ring::PAGE_SIZE;
use zerocopy::AsBytes;
use zerocopy::FromZeroes;

pub struct TestWorker {
    task: Task<Result<(), WorkerError>>,
}

impl TestWorker {
    pub async fn teardown(self) -> Result<(), WorkerError> {
        self.task.await
    }

    pub fn start<T: ring::RingMem + 'static + Sync>(
        controller: ScsiController,
        spawner: impl Spawn,
        mem: GuestMemory,
        channel: RawAsyncChannel<T>,
        io_queue_depth: Option<u32>,
    ) -> Self {
        let task = spawner.spawn("test", async move {
            let mut worker = Worker::new(
                controller.state.clone(),
                channel,
                0,
                mem,
                Default::default(),
                io_queue_depth.unwrap_or(256),
                Arc::new(Protocol {
                    state: RwLock::new(ProtocolState::Init(InitState::Begin)),
                    ready: Default::default(),
                }),
                None,
            )
            .unwrap();
            worker.process_primary().await
        });

        Self { task }
    }
}

pub(crate) fn parse_guest_completion_check_flags_status<T: ring::RingMem>(
    packet: &IncomingPacket<'_, T>,
    flags: u32,
    status: protocol::NtStatus,
) -> Result<(), PacketError> {
    match packet {
        IncomingPacket::Completion(compl) => {
            let mut reader = compl.reader();
            let header: protocol::Packet = reader.read_plain().map_err(PacketError::Access)?;
            assert_eq!(header.flags, flags, "mismatched flags");
            assert_eq!(header.status, status, "mismatched status");
            assert_eq!(
                header.operation,
                protocol::Operation::COMPLETE_IO,
                "mismatched operation"
            );
            Ok(())
        }
        IncomingPacket::Data(_) => Err(PacketError::InvalidPacketType),
    }
}

pub(crate) fn parse_guest_completion<T: ring::RingMem>(
    packet: &IncomingPacket<'_, T>,
) -> Result<(), PacketError> {
    parse_guest_completion_check_flags_status(packet, 0, protocol::NtStatus::SUCCESS)
}

pub(crate) fn parse_guest_completed_io<T: ring::RingMem>(
    packet: &IncomingPacket<'_, T>,
    expected_srb_status: SrbStatus,
) -> Result<(), PacketError> {
    parse_guest_completed_io_check_tx_len(packet, expected_srb_status, None)
}

pub(crate) fn parse_guest_completed_io_check_tx_len<T: ring::RingMem>(
    packet: &IncomingPacket<'_, T>,
    expected_srb_status: SrbStatus,
    expected_data_tx_length: Option<usize>,
) -> Result<(), PacketError> {
    match packet {
        IncomingPacket::Completion(compl) => {
            let mut reader = compl.reader();
            let header: protocol::Packet = reader.read_plain().map_err(PacketError::Access)?;
            if header.operation != protocol::Operation::COMPLETE_IO {
                Err(PacketError::UnrecognizedOperation(header.operation))
            } else {
                if expected_srb_status == SrbStatus::SUCCESS {
                    assert_eq!(header.status, protocol::NtStatus::SUCCESS);
                    if let Some(expected_data_tx_length) = expected_data_tx_length {
                        let payload: protocol::ScsiRequest =
                            reader.read_plain().map_err(PacketError::Access)?;
                        assert_eq!(
                            payload.data_transfer_length as usize,
                            expected_data_tx_length
                        );
                    }
                } else {
                    assert_ne!(header.status, protocol::NtStatus::SUCCESS);
                    let payload: protocol::ScsiRequest =
                        reader.read_plain().map_err(PacketError::Access)?;
                    assert_eq!(payload.srb_status.status(), expected_srb_status);
                }
                Ok(())
            }
        }
        _ => Err(PacketError::InvalidPacketType),
    }
}

pub(crate) fn parse_guest_enumerate_bus<T: ring::RingMem>(
    packet: &IncomingPacket<'_, T>,
) -> Result<(), PacketError> {
    match packet {
        IncomingPacket::Data(p) => {
            let mut reader = p.reader();
            let header: protocol::Packet = reader.read_plain().map_err(PacketError::Access)?;
            if header.operation != protocol::Operation::ENUMERATE_BUS {
                Err(PacketError::UnrecognizedOperation(header.operation))
            } else {
                assert_eq!(header.status, protocol::NtStatus::SUCCESS);
                Ok(())
            }
        }
        _ => Err(PacketError::InvalidPacketType),
    }
}

pub struct TestGuest {
    pub queue: Queue<FlatRingMem>,
    pub transaction_id: u64,
}

impl TestGuest {
    pub async fn send_data_packet_sync(&mut self, payload: &[&[u8]]) {
        self.queue
            .split()
            .1
            .write(OutgoingPacket {
                packet_type: OutgoingPacketType::InBandWithCompletion,
                transaction_id: self.transaction_id,
                payload,
            })
            .await
            .unwrap();

        self.transaction_id += 1;
    }

    pub async fn send_gpa_direct_packet_sync(
        &mut self,
        payload: &[&[u8]],
        gpa_start: u64,
        byte_len: usize,
    ) {
        let start_page: u64 = gpa_start / PAGE_SIZE as u64;
        let end_page: u64 = (gpa_start + (byte_len + PAGE_SIZE - 1) as u64) / PAGE_SIZE as u64;
        let gpas: Vec<u64> = (start_page..end_page).collect();
        let pages =
            PagedRange::new(gpa_start as usize % PAGE_SIZE, byte_len, gpas.as_slice()).unwrap();
        self.queue
            .split()
            .1
            .write(OutgoingPacket {
                packet_type: OutgoingPacketType::GpaDirect(&[pages]),
                transaction_id: self.transaction_id,
                payload,
            })
            .await
            .unwrap();

        self.transaction_id += 1;
    }

    // This function assumes the sector size is 512.
    pub async fn send_write_packet(
        &mut self,
        path: ScsiPath,
        buf_gpa: u64,
        block: u32,
        byte_len: usize,
    ) {
        let write_packet = protocol::Packet {
            operation: protocol::Operation::EXECUTE_SRB,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };

        let cdb = scsi::Cdb10 {
            operation_code: ScsiOp::WRITE,
            logical_block: block.into(),
            transfer_blocks: ((byte_len / 512) as u16).into(),
            ..FromZeroes::new_zeroed()
        };

        let mut scsi_req = protocol::ScsiRequest {
            target_id: path.target,
            path_id: path.path,
            lun: path.lun,
            length: protocol::SCSI_REQUEST_LEN_V2 as u16,
            cdb_length: size_of::<scsi::Cdb10>() as u8,
            data_transfer_length: byte_len as u32,
            ..FromZeroes::new_zeroed()
        };

        scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());

        // send the gpa packet
        self.send_gpa_direct_packet_sync(
            &[write_packet.as_bytes(), scsi_req.as_bytes()],
            buf_gpa,
            byte_len,
        )
        .await;
    }

    // This function assumes the sector size is 512.
    pub async fn send_read_packet(
        &mut self,
        path: ScsiPath,
        read_gpa: u64,
        block: u32,
        byte_len: usize,
    ) {
        let read_packet = protocol::Packet {
            operation: protocol::Operation::EXECUTE_SRB,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };

        let cdb = scsi::Cdb10 {
            operation_code: ScsiOp::READ,
            logical_block: block.into(),
            transfer_blocks: ((byte_len / 512) as u16).into(),
            ..FromZeroes::new_zeroed()
        };

        let mut scsi_req = protocol::ScsiRequest {
            target_id: path.target,
            path_id: path.path,
            lun: path.lun,
            length: protocol::SCSI_REQUEST_LEN_V2 as u16,
            cdb_length: size_of::<scsi::Cdb10>() as u8,
            data_transfer_length: byte_len as u32,
            data_in: 1,
            ..FromZeroes::new_zeroed()
        };

        scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());

        // send the gpa packet
        self.send_gpa_direct_packet_sync(
            &[read_packet.as_bytes(), scsi_req.as_bytes()],
            read_gpa,
            byte_len,
        )
        .await;
    }

    pub async fn send_report_luns_packet(
        &mut self,
        path: ScsiPath,
        data_buffer_gpa: u64,
        data_buffer_len: usize,
    ) {
        let packet = protocol::Packet {
            operation: protocol::Operation::EXECUTE_SRB,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };

        let cdb = scsi::Cdb10 {
            operation_code: ScsiOp::REPORT_LUNS,
            ..FromZeroes::new_zeroed()
        };

        let mut scsi_req = protocol::ScsiRequest {
            target_id: path.target,
            path_id: path.path,
            lun: path.lun,
            length: protocol::SCSI_REQUEST_LEN_V2 as u16,
            cdb_length: size_of::<scsi::Cdb10>() as u8,
            data_transfer_length: data_buffer_len as u32,
            data_in: 1,
            ..FromZeroes::new_zeroed()
        };

        scsi_req.payload[0..10].copy_from_slice(cdb.as_bytes());

        self.send_gpa_direct_packet_sync(
            &[packet.as_bytes(), scsi_req.as_bytes()],
            data_buffer_gpa,
            data_buffer_len,
        )
        .await;
    }

    pub(crate) async fn verify_completion<F>(&mut self, f: F)
    where
        F: Clone + FnOnce(&IncomingPacket<'_, FlatRingMem>) -> Result<(), PacketError>,
    {
        let (mut reader, _) = self.queue.split();
        let packet = reader.read().await.unwrap();
        f(&packet).unwrap();
    }

    // Send protocol negotiation packets for a test guest.
    pub async fn perform_protocol_negotiation(&mut self) {
        let negotiate_packet = protocol::Packet {
            operation: protocol::Operation::BEGIN_INITIALIZATION,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        self.send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        self.verify_completion(parse_guest_completion).await;

        let version_packet = protocol::Packet {
            operation: protocol::Operation::QUERY_PROTOCOL_VERSION,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        let version = protocol::ProtocolVersion {
            major_minor: protocol::VERSION_BLUE,
            reserved: 0,
        };
        self.send_data_packet_sync(&[version_packet.as_bytes(), version.as_bytes()])
            .await;
        self.verify_completion(parse_guest_completion).await;

        let properties_packet = protocol::Packet {
            operation: protocol::Operation::QUERY_PROPERTIES,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        self.send_data_packet_sync(&[properties_packet.as_bytes()])
            .await;
        self.verify_completion(parse_guest_completion).await;

        let negotiate_packet = protocol::Packet {
            operation: protocol::Operation::END_INITIALIZATION,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        self.send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        self.verify_completion(parse_guest_completion).await;
    }

    pub(crate) async fn verify_graceful_close(self, worker: TestWorker) {
        drop(self);
        match worker.task.await {
            Err(WorkerError::Queue(err)) if err.is_closed_error() => (),
            _ => panic!("Worker thread did not complete gracefully!"),
        }
    }
}
