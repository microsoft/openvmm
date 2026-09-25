// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests that drive the console through the virtio-mmio transport, as a
//! guest's driver does.

use super::DATA_BASE;
use super::QUEUE_SIZE;
use super::RX_AVAIL_ADDR;
use super::RX_DESC_ADDR;
use super::RX_USED_ADDR;
use super::TX_AVAIL_ADDR;
use super::TX_DESC_ADDR;
use super::TX_USED_ADDR;
use super::TestHarness;
use super::harness::yield_now;
use chipset_device::io::IoResult;
use chipset_device::mmio::MmioIntercept;
use chipset_device::poll_device::PollDevice;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::timer::PolledTimer;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;
use test_with_tracing::test;
use virtio::spec::queue::DescriptorFlags;
use virtio::test_helpers::init_avail_ring;
use virtio::test_helpers::init_used_ring;
use virtio::test_helpers::make_available;
use virtio::test_helpers::read_used;
use virtio::test_helpers::write_descriptor;
use virtio::transport::VirtioMmioDevice;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;

// The virtio-mmio registers that a guest's driver writes (virtio 1.2, section
// 4.2.2).
const DRIVER_FEATURES: u64 = 0x20;
const DRIVER_FEATURES_SEL: u64 = 0x24;
const QUEUE_SEL: u64 = 0x30;
const QUEUE_NUM: u64 = 0x38;
const QUEUE_READY: u64 = 0x44;
const QUEUE_NOTIFY: u64 = 0x50;
const STATUS: u64 = 0x70;
const QUEUE_DESC_LOW: u64 = 0x80;
const QUEUE_DRIVER_LOW: u64 = 0x90;
const QUEUE_DEVICE_LOW: u64 = 0xa0;

// The device status bits (virtio 1.2, section 2.1).
const ACKNOWLEDGE: u32 = 1;
const DRIVER: u32 = 2;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;

/// The console behind the virtio-mmio transport, and the guest memory of its
/// rings, which lie where the harness puts them.
struct MmioConsole {
    transport: VirtioMmioDevice,
    mem: GuestMemory,
}

impl MmioConsole {
    /// Writes `value` to the transport register at `offset`, as a vCPU does,
    /// and waits for the write to complete, polling the transport as the
    /// chipset does.
    async fn write(&mut self, offset: u64, value: u32) {
        let mut token = match self.transport.mmio_write(offset, &value.to_le_bytes()) {
            IoResult::Ok => return,
            IoResult::Defer(token) => token,
            IoResult::Err(error) => panic!("register write failed: {error:?}"),
        };
        for _ in 0..1000 {
            let mut cx = Context::from_waker(Waker::noop());
            self.transport.poll_device(&mut cx);
            if let Poll::Ready(result) = token.poll_write(&mut cx) {
                result.unwrap();
                return;
            }
            yield_now().await;
        }
        panic!("register write did not complete");
    }

    /// Initializes the device as a guest's driver does, up to DRIVER_OK, with
    /// empty rings.
    async fn initialize(&mut self) {
        for (avail, used) in [(RX_AVAIL_ADDR, RX_USED_ADDR), (TX_AVAIL_ADDR, TX_USED_ADDR)] {
            init_avail_ring(&self.mem, avail);
            init_used_ring(&self.mem, used);
        }
        self.write(STATUS, ACKNOWLEDGE).await;
        self.write(STATUS, ACKNOWLEDGE | DRIVER).await;
        // VIRTIO_F_VERSION_1, bit 0 of the second feature bank.
        self.write(DRIVER_FEATURES_SEL, 1).await;
        self.write(DRIVER_FEATURES, 1).await;
        self.write(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK).await;
        for (queue, desc, avail, used) in [
            (0, RX_DESC_ADDR, RX_AVAIL_ADDR, RX_USED_ADDR),
            (1, TX_DESC_ADDR, TX_AVAIL_ADDR, TX_USED_ADDR),
        ] {
            self.write(QUEUE_SEL, queue).await;
            self.write(QUEUE_NUM, QUEUE_SIZE.into()).await;
            self.write(QUEUE_DESC_LOW, desc.try_into().unwrap()).await;
            self.write(QUEUE_DRIVER_LOW, avail.try_into().unwrap())
                .await;
            self.write(QUEUE_DEVICE_LOW, used.try_into().unwrap()).await;
            self.write(QUEUE_READY, 1).await;
        }
        self.write(STATUS, ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK)
            .await;
    }

    /// Makes a receive buffer of `len` bytes at the start of the data area
    /// available in descriptor 0, and notifies the receive queue.
    async fn post_rx_buffer(&mut self, len: u32) {
        write_descriptor(
            &self.mem,
            RX_DESC_ADDR,
            0,
            DATA_BASE,
            len,
            DescriptorFlags::new().with_write(true),
            0,
        );
        let mut avail_idx = 0;
        make_available(&self.mem, RX_AVAIL_ADDR, QUEUE_SIZE, 0, &mut avail_idx);
        self.write(QUEUE_NOTIFY, 0).await;
    }
}

/// A guest that writes 0 to the status of the running console while host
/// input is gated resets the device, and its driver then starts it again: the
/// console receives no host input until the gate is released.
#[async_test]
async fn guest_status_reset_keeps_host_input_gated(driver: DefaultDriver) {
    let harness = TestHarness::new(&driver);
    let handle = harness.handle;
    let mut console = MmioConsole {
        transport: VirtioMmioDevice::new(
            Box::new(harness.device),
            &driver,
            harness.mem.clone(),
            LineInterrupt::detached(),
            None,
            0,
            0x1000,
        )
        .unwrap(),
        mem: harness.mem,
    };
    console.initialize().await;
    ChangeDeviceState::quiesce_input(&mut console.transport)
        .await
        .unwrap();

    console.write(STATUS, 0).await;
    console.initialize().await;
    handle.inject_rx_data(b"gated-input");
    console.post_rx_buffer(64).await;
    let mut timer = PolledTimer::new(&driver);
    timer.sleep(Duration::from_millis(100)).await;
    let mut used_idx = 0;
    assert_eq!(
        read_used(&console.mem, RX_USED_ADDR, QUEUE_SIZE, &mut used_idx),
        None
    );
    assert_eq!(handle.pending_rx_len(), b"gated-input".len());

    ChangeDeviceState::resume_input(&mut console.transport)
        .await
        .unwrap();
    let mut used = None;
    for _ in 0..500 {
        used = read_used(&console.mem, RX_USED_ADDR, QUEUE_SIZE, &mut used_idx);
        if used.is_some() {
            break;
        }
        timer.sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(used, Some((0, b"gated-input".len() as u32)));
    let mut received = [0; b"gated-input".len()];
    console.mem.read_at(DATA_BASE, &mut received).unwrap();
    assert_eq!(&received, b"gated-input");
}
