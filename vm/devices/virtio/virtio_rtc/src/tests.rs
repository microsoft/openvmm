// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_event::Event;
use test_with_tracing::test;
use virtio::QueueResources;
use virtio::queue::QueueParams;
use virtio::spec::VirtioDeviceFeatures;
use virtio::spec::queue::DescriptorFlags;
use virtio::test_helpers::init_avail_ring;
use virtio::test_helpers::init_used_ring;
use virtio::test_helpers::make_available;
use virtio::test_helpers::wait_for_used;
use virtio::test_helpers::write_descriptor;
use vmcore::interrupt::Interrupt;
use vmcore::vm_task::SingleDriverBackend;

const QUEUE_SIZE: u16 = 16;
const DESC_ADDR: u64 = 0;
const AVAIL_ADDR: u64 = 0x1000;
const USED_ADDR: u64 = 0x2000;
const REQUEST_ADDR: u64 = 0x10000;
const RESPONSE_ADDR: u64 = 0x11000;
const TOTAL_MEM_SIZE: usize = 0x20000;

struct TestHarness {
    device: VirtioRtcDevice,
    mem: GuestMemory,
    driver: DefaultDriver,
    queue_event: Event,
    interrupt_event: Event,
    avail_idx: u16,
    used_idx: u16,
}

impl TestHarness {
    fn new(driver: &DefaultDriver) -> Self {
        let mem = GuestMemory::allocate(TOTAL_MEM_SIZE);
        init_avail_ring(&mem, AVAIL_ADDR);
        init_used_ring(&mem, USED_ADDR);

        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let device = VirtioRtcDevice::new(&driver_source);

        Self {
            device,
            mem,
            driver: driver.clone(),
            queue_event: Event::new(),
            interrupt_event: Event::new(),
            avail_idx: 0,
            used_idx: 0,
        }
    }

    async fn enable(&mut self) {
        self.device
            .start_queue(
                0,
                QueueResources {
                    params: QueueParams {
                        size: QUEUE_SIZE,
                        enable: true,
                        desc_addr: DESC_ADDR,
                        avail_addr: AVAIL_ADDR,
                        used_addr: USED_ADDR,
                    },
                    notify: Interrupt::from_event(self.interrupt_event.clone()),
                    event: self.queue_event.clone(),
                    guest_memory: self.mem.clone(),
                },
                &VirtioDeviceFeatures::new(),
                None,
            )
            .await
            .unwrap();
    }

    async fn submit_and_wait(
        &mut self,
        request_addr: u64,
        readable_len: u32,
        writable_len: u32,
    ) -> (u32, Vec<u8>) {
        self.mem
            .write_at(RESPONSE_ADDR, &vec![0xaa; writable_len as usize + 1])
            .unwrap();

        write_descriptor(
            &self.mem,
            DESC_ADDR,
            0,
            request_addr,
            readable_len,
            DescriptorFlags::new().with_next(true),
            1,
        );
        write_descriptor(
            &self.mem,
            DESC_ADDR,
            1,
            RESPONSE_ADDR,
            writable_len,
            DescriptorFlags::new().with_write(true),
            0,
        );
        make_available(&self.mem, AVAIL_ADDR, QUEUE_SIZE, 0, &mut self.avail_idx);
        self.queue_event.signal();

        let (id, written) = wait_for_used(
            &self.driver,
            &self.interrupt_event,
            &self.mem,
            USED_ADDR,
            QUEUE_SIZE,
            &mut self.used_idx,
        )
        .await;
        assert_eq!(id, 0);

        let mut response = vec![0; writable_len as usize];
        if writable_len != 0 {
            self.mem.read_at(RESPONSE_ADDR, &mut response).unwrap();
        }
        (written, response)
    }
}

fn head(msg_type: u16) -> spec::ReqHead {
    spec::ReqHead {
        msg_type: virtio::spec::u16_le::new(msg_type),
        reserved: [0; 6],
    }
}

fn read_request(clock_id: u16) -> spec::ReqClock {
    spec::ReqClock {
        head: head(spec::REQ_READ),
        clock_id: virtio::spec::u16_le::new(clock_id),
        reserved: [0; 6],
    }
}

#[async_test]
async fn rtc_reports_correct_traits(driver: DefaultDriver) {
    let harness = TestHarness::new(&driver);
    let traits = harness.device.traits();
    assert_eq!(traits.device_id, virtio::spec::VirtioDeviceType::RTC);
    assert_eq!(traits.max_queues, 1);
    assert_eq!(traits.device_register_length, 0);
    assert_eq!(traits.device_features.device_specific_low(), 0);
}

#[async_test]
async fn rtc_reads_host_time(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    let request = read_request(0);
    harness
        .mem
        .write_at(REQUEST_ADDR, request.as_bytes())
        .unwrap();

    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let (written, response) = harness
        .submit_and_wait(
            REQUEST_ADDR,
            size_of::<spec::ReqClock>() as u32,
            size_of::<spec::RespRead>() as u32,
        )
        .await;
    let after = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    assert_eq!(written, size_of::<spec::RespRead>() as u32);
    let response = spec::RespRead::read_from_bytes(&response).unwrap();
    assert_eq!(response.head.status, spec::S_OK);
    assert_eq!(response.head.reserved, [0; 7]);
    let reading = u128::from(response.clock_reading.get());
    assert!((before.min(after)..=before.max(after)).contains(&reading));
    harness.device.stop_queue(0).await.unwrap();
}

#[async_test]
async fn rtc_reports_clock_capabilities(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    let cfg = head(spec::REQ_CFG);
    harness.mem.write_at(REQUEST_ADDR, cfg.as_bytes()).unwrap();
    let (written, response) = harness
        .submit_and_wait(
            REQUEST_ADDR,
            size_of::<spec::ReqHead>() as u32,
            size_of::<spec::RespCfg>() as u32,
        )
        .await;
    assert_eq!(written, size_of::<spec::RespCfg>() as u32);
    let response = spec::RespCfg::read_from_bytes(&response).unwrap();
    assert_eq!(response.head.status, spec::S_OK);
    assert_eq!(response.num_clocks.get(), 1);

    let clock_cap = spec::ReqClock {
        head: head(spec::REQ_CLOCK_CAP),
        clock_id: virtio::spec::u16_le::new(0),
        reserved: [0; 6],
    };
    harness
        .mem
        .write_at(REQUEST_ADDR, clock_cap.as_bytes())
        .unwrap();
    let (written, response) = harness
        .submit_and_wait(
            REQUEST_ADDR,
            size_of::<spec::ReqClock>() as u32,
            size_of::<spec::RespClockCap>() as u32,
        )
        .await;
    assert_eq!(written, size_of::<spec::RespClockCap>() as u32);
    let response = spec::RespClockCap::read_from_bytes(&response).unwrap();
    assert_eq!(response.head.status, spec::S_OK);
    assert_eq!(response.clock_type, spec::CLOCK_UTC_MAYBE_SMEARED);
    assert_eq!(response.leap_second_smearing, spec::SMEAR_UNSPECIFIED);
    assert_eq!(response.flags, 0);
    assert_eq!(response.reserved, [0; 5]);
    harness.device.stop_queue(0).await.unwrap();
}

#[async_test]
async fn rtc_handles_cross_requests(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;

    for (msg_type, response_size, known_counter_status) in [
        (
            spec::REQ_CROSS_CAP,
            size_of::<spec::RespCrossCap>(),
            spec::S_OK,
        ),
        (
            spec::REQ_READ_CROSS,
            size_of::<spec::RespReadCross>(),
            spec::S_EOPNOTSUPP,
        ),
    ] {
        for (counter, expected_status) in [
            (spec::COUNTER_ARM_VCT, known_counter_status),
            (spec::COUNTER_X86_TSC, known_counter_status),
            (spec::COUNTER_INVALID, spec::S_EINVAL),
            (2, spec::S_EOPNOTSUPP),
        ] {
            let request = spec::ReqCross {
                head: head(msg_type),
                clock_id: virtio::spec::u16_le::new(0),
                hw_counter: counter,
                reserved: [0; 5],
            };
            harness
                .mem
                .write_at(REQUEST_ADDR, request.as_bytes())
                .unwrap();
            let (written, response) = harness
                .submit_and_wait(
                    REQUEST_ADDR,
                    size_of::<spec::ReqCross>() as u32,
                    response_size as u32,
                )
                .await;
            assert_eq!(written, response_size as u32);
            assert_eq!(
                response[0], expected_status,
                "message {msg_type:#06x}, counter {counter:#04x}"
            );
            if msg_type == spec::REQ_CROSS_CAP && expected_status == spec::S_OK {
                let response = spec::RespCrossCap::read_from_bytes(&response).unwrap();
                assert_eq!(response.flags, 0);
                assert_eq!(response.head.reserved, [0; 7]);
                assert_eq!(response.reserved, [0; 7]);
            }
        }
    }
    harness.device.stop_queue(0).await.unwrap();
}

#[async_test]
async fn rtc_rejects_unknown_clock(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    let request = read_request(u16::MAX);
    harness
        .mem
        .write_at(REQUEST_ADDR, request.as_bytes())
        .unwrap();
    let (written, response) = harness
        .submit_and_wait(
            REQUEST_ADDR,
            size_of::<spec::ReqClock>() as u32,
            size_of::<spec::RespRead>() as u32,
        )
        .await;
    assert_eq!(written, size_of::<spec::RespRead>() as u32);
    assert_eq!(response[0], spec::S_ENODEV);
    harness.device.stop_queue(0).await.unwrap();
}

#[async_test]
async fn rtc_handles_short_response_buffers(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    let request = read_request(0);
    harness
        .mem
        .write_at(REQUEST_ADDR, request.as_bytes())
        .unwrap();

    for writable_len in 0..size_of::<spec::RespRead>() as u32 {
        let (written, response) = harness
            .submit_and_wait(
                REQUEST_ADDR,
                size_of::<spec::ReqClock>() as u32,
                writable_len,
            )
            .await;
        assert_eq!(
            written,
            writable_len.min(size_of::<spec::RespHead>() as u32)
        );
        if writable_len != 0 {
            assert_eq!(response[0], spec::S_EINVAL);
            assert!(response[1..written as usize].iter().all(|&byte| byte == 0));
            assert!(
                response[written as usize..]
                    .iter()
                    .all(|&byte| byte == 0xaa)
            );
        }
        let mut after_buffer = [0; 1];
        harness
            .mem
            .read_at(RESPONSE_ADDR + u64::from(writable_len), &mut after_buffer)
            .unwrap();
        assert_eq!(after_buffer, [0xaa]);
    }

    harness.device.stop_queue(0).await.unwrap();
}

#[async_test]
async fn rtc_ignores_trailing_request_bytes(driver: DefaultDriver) {
    let mut harness = TestHarness::new(&driver);
    harness.enable().await;
    let cfg = head(spec::REQ_CFG);
    let read = read_request(0);

    for request in [cfg.as_bytes(), read.as_bytes()] {
        let request_addr = (TOTAL_MEM_SIZE - request.len()) as u64;
        harness.mem.write_at(request_addr, request).unwrap();
        let (written, response) = harness
            .submit_and_wait(request_addr, MAX_REQUEST_SIZE as u32, 32)
            .await;
        assert_eq!(written, 16);
        assert_eq!(response[0], spec::S_OK);
        assert!(
            response[written as usize..]
                .iter()
                .all(|&byte| byte == 0xaa)
        );
    }
    harness.device.stop_queue(0).await.unwrap();
}
