// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test instructure for criterion benchmarks.

use crate::protocol;
use crate::test_helpers;
use crate::test_helpers::TestGuest;
use crate::test_helpers::TestWorker;
use crate::ScsiController;
use crate::ScsiControllerDisk;
use crate::ScsiPath;
use disklayer_ram::ram_disk;
use guestmem::GuestMemory;
use pal_async::driver::SpawnDriver;
use scsi_defs::srb::SrbStatus;
use scsidisk::SimpleScsiDisk;
use std::sync::Arc;
use vmbus_async::queue::Queue;
use vmbus_channel::connected_async_channels;
use zerocopy::IntoBytes;

pub struct PerfTester {
    _worker: TestWorker,
    guest: TestGuest,
}

impl PerfTester {
    pub async fn new(driver: impl SpawnDriver + Clone) -> Self {
        let io_queue_depth = None;
        let device = ram_disk(64 * 1024, true).unwrap();
        let controller = ScsiController::new();
        let disk =
            ScsiControllerDisk::new(Arc::new(SimpleScsiDisk::new(device, Default::default())));
        controller
            .attach(
                ScsiPath {
                    path: 0,
                    target: 0,
                    lun: 0,
                },
                disk,
            )
            .unwrap();

        let (host, guest) = connected_async_channels(16 * 1024);
        let guest_queue = Queue::new(guest).unwrap();

        let mut guest = TestGuest {
            queue: guest_queue,
            transaction_id: 0,
        };

        let test_guest_mem = GuestMemory::allocate(16 * 1024);

        let worker = TestWorker::start(
            controller,
            driver,
            test_guest_mem.clone(),
            host,
            io_queue_depth,
        );

        let negotiate_packet = protocol::Packet {
            operation: protocol::Operation::BEGIN_INITIALIZATION,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest
            .verify_completion(test_helpers::parse_guest_completion)
            .await;

        let version_packet = protocol::Packet {
            operation: protocol::Operation::QUERY_PROTOCOL_VERSION,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        let version = protocol::ProtocolVersion {
            major_minor: protocol::VERSION_BLUE,
            reserved: 0,
        };
        guest
            .send_data_packet_sync(&[version_packet.as_bytes(), version.as_bytes()])
            .await;
        guest
            .verify_completion(test_helpers::parse_guest_completion)
            .await;

        let properties_packet = protocol::Packet {
            operation: protocol::Operation::QUERY_PROPERTIES,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[properties_packet.as_bytes()])
            .await;
        guest
            .verify_completion(test_helpers::parse_guest_completion)
            .await;

        let negotiate_packet = protocol::Packet {
            operation: protocol::Operation::END_INITIALIZATION,
            flags: 0,
            status: protocol::NtStatus::SUCCESS,
        };
        guest
            .send_data_packet_sync(&[negotiate_packet.as_bytes()])
            .await;
        guest
            .verify_completion(test_helpers::parse_guest_completion)
            .await;

        Self {
            guest,
            _worker: worker,
        }
    }

    pub async fn read(&mut self, count: usize) {
        const IO_LEN: usize = 4 * 1024;
        for _ in 0..count {
            self.guest
                .send_read_packet(ScsiPath::default(), 0, 1, IO_LEN)
                .await;
        }
        for _ in 0..count {
            self.guest
                .verify_completion(|p| {
                    test_helpers::parse_guest_completed_io(p, SrbStatus::SUCCESS)
                })
                .await;
        }
    }
}
