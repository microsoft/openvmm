// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests which exercise TDISP functionality of a relayed VPCI device.
//! These tests ensure at least basic smoke test of TDISP machinery driven by
//! the guest with VPCI. This ensures that a TDISP device behaves correctly in a
//! relayed VPCI environment.

#![cfg(test)]

use super::RelayedVpciDevice;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::mmio::ExternallyManagedMmioIntercepts;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigByteEnable;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use closeable_mutex::CloseableMutex;
use guestmem::GuestMemory;
use guid::Guid;
use hvdef::Vtl;
use openhcl_tdisp::noop::TdispNoopResourceValidator;
use pal_async::DefaultDriver;
use pal_async::async_test;
use pal_async::task::Spawn;
use parking_lot::Mutex;
use pci_core::spec::cfg_space::Command;
use pci_core::spec::cfg_space::HeaderType00;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::task::Waker;
use task_control::StopTask;
use tdisp::TdispHostDeviceTargetEmulator;
use tdisp::TdispTdiState;
use tdisp::test_helpers::new_null_tdisp_interface;
use test_with_tracing::test;
use virt::IsolationType;
use vmbus_channel::simple::SimpleVmbusDevice;
use vmcore::vpci_msi::VpciInterruptMapper;
use vpci::bus::VpciBusConfig;
use vpci::bus::VpciBusDevice;
use vpci::test_helpers::TestVpciInterruptController;
use vpci_client::MemoryAccess;
use vpci_client::VpciClient;
use vpci_client::VpciDevice;

/// A config space register unrelated to TDISP, used to check that writes that
/// would otherwise pass straight through still wait their turn.
const SCRATCH_OFFSET: u16 = 0x40;

/// The VTOM most SNP platforms report, which TDISP setup expects to be present.
const TEST_VTOM: u64 = 0x400000000000;

/// The config space the emulated host device presents, plus a log of everything
/// the relay has pushed through to it.
#[derive(Default)]
struct HostConfigSpace {
    /// Every write that reached the device, in the order it arrived, as
    /// (offset, value) pairs.
    writes: Vec<(u16, u32)>,
    /// The registers written so far, keyed by DWORD-aligned offset.
    registers: HashMap<u16, u32>,
}

/// The device on the far side of the relayed VPCI bus. It answers config space
/// accesses out of a plain register map and records every write, so a test can
/// tell exactly what reached the device and when.
struct TestHostDevice {
    tdisp_interface: TdispHostDeviceTargetEmulator,
    cfg: Arc<Mutex<HostConfigSpace>>,
}

impl ChipsetDevice for TestHostDevice {
    fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
        Some(self)
    }

    fn supports_tdisp_host(&mut self) -> Option<&mut dyn tdisp::TdispHostDeviceTarget> {
        Some(&mut self.tdisp_interface)
    }
}

impl PciConfigSpace for TestHostDevice {
    fn pci_cfg_read(&mut self, offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
        value.set(self.cfg.lock().registers.get(&offset).copied().unwrap_or(0));
        IoResult::Ok
    }

    fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> IoResult {
        let mut cfg = self.cfg.lock();
        cfg.writes.push((offset, value.extract()));
        // The device implements no BARs, so BAR writes are dropped and the
        // registers keep reading back as zero. The client probes them for size
        // during bring-up and would otherwise see the probe value itself.
        if !(HeaderType00::BAR0.0..HeaderType00::BAR5.0 + 4).contains(&offset) {
            let current = cfg.registers.get(&offset).copied().unwrap_or(0);
            cfg.registers.insert(offset, value.merge(current));
        }
        IoResult::Ok
    }
}

/// Bridges the client's MMIO window onto the bus device directly, with no
/// address space in between.
struct BusWrapper(VpciBusDevice);

impl MemoryAccess for BusWrapper {
    fn gpa(&mut self) -> u64 {
        0x123456780000
    }

    fn read(&mut self, addr: u64, value: &mut [u8]) {
        self.0
            .supports_mmio()
            .unwrap()
            .mmio_read(addr, value)
            .unwrap();
    }

    fn write(&mut self, addr: u64, value: &[u8]) {
        self.0
            .supports_mmio()
            .unwrap()
            .mmio_write(addr, value)
            .unwrap();
    }
}

/// Everything a test needs to drive one relayed device.
struct TestRelay {
    /// The device under test, behind the same kind of lock the chipset uses.
    relay: Mutex<RelayedVpciDevice>,
    /// The client's handle to the same device, for checking TDI state.
    device: Arc<VpciDevice>,
    /// The emulated host device's config space and write log.
    cfg: Arc<Mutex<HostConfigSpace>>,
    /// Keeps the VPCI channel server running for the lifetime of the test.
    _server: pal_async::task::Task<()>,
    /// Keeps the client alive for the lifetime of the test.
    _client: VpciClient,
}

/// Brings up a VPCI bus with an emulated TDISP-capable device on it and wraps
/// the resulting client device in the relay under test.
///
/// `tdisp_capable` sets whether the relay treats the device as TDISP capable,
/// which the relay normally decides by probing the device when the host offers
/// it.
async fn connect_relay(driver: &DefaultDriver, tdisp_capable: bool) -> TestRelay {
    let cfg = Arc::new(Mutex::new(HostConfigSpace::default()));
    let host_device = Arc::new(CloseableMutex::new(TestHostDevice {
        tdisp_interface: new_null_tdisp_interface("vpci-relay-unit-test"),
        cfg: cfg.clone(),
    }));

    let (bus, mut channel) = VpciBusDevice::new(
        VpciBusConfig {
            instance_id: Guid::new_random(),
            vtom: Some(TEST_VTOM),
            vnode: None,
        },
        host_device,
        &mut ExternallyManagedMmioIntercepts,
        VpciInterruptMapper::new(TestVpciInterruptController::new()),
    )
    .unwrap();

    let (host, guest) = vmbus_channel::connected_async_channels(32768);
    let mut runner = channel.open(host, GuestMemory::empty()).unwrap();
    let server = driver.spawn("vpci-server", async move {
        StopTask::run_with(std::future::pending(), async |stop| {
            let _ = channel.run(stop, &mut runner).await;
        })
        .await
    });

    let (client, devices) =
        VpciClient::connect(driver, guest, Box::new(BusWrapper(bus)), mesh::channel().0)
            .await
            .unwrap();

    let (device, _removed) = devices
        .into_iter()
        .next()
        .unwrap()
        .init(
            Arc::new(TdispNoopResourceValidator::new()),
            IsolationType::Snp,
            Vtl::Vtl0,
        )
        .await
        .unwrap();
    let device = Arc::new(device);

    // Drop everything the client did while bringing the device up so that each
    // test sees only its own writes.
    cfg.lock().writes.clear();

    TestRelay {
        relay: Mutex::new(RelayedVpciDevice {
            device: device.clone(),
            pending: None,
            queued: VecDeque::new(),
            waker: Waker::noop().clone(),
            tdisp_capable,
        }),
        device,
        cfg,
        _server: server,
        _client: client,
    }
}

impl TestRelay {
    /// Issues a config space write and returns its result, holding the device
    /// lock only for the duration of the call, as the chipset does.
    fn write(&self, offset: u16, value: u32, byte_enable: PciConfigByteEnable) -> IoResult {
        self.relay
            .lock()
            .pci_cfg_write(offset, ByteEnabledDwordWrite::new(value, byte_enable))
    }

    /// Issues a config space write that is expected to be deferred, and returns
    /// the token to wait on.
    fn deferred_write(
        &self,
        offset: u16,
        value: u32,
        byte_enable: PciConfigByteEnable,
    ) -> DeferredToken {
        match self.write(offset, value, byte_enable) {
            IoResult::Defer(token) => token,
            other => panic!("expected offset {offset:#x} to defer, got {other:?}"),
        }
    }

    /// Issues a config space read and returns the value.
    fn read(&self, offset: u16) -> u32 {
        let mut value = 0;
        self.relay
            .lock()
            .pci_cfg_read(
                offset,
                ByteEnabledDwordRead::with_all_bytes_enabled(&mut value),
            )
            .unwrap();
        value
    }

    /// Runs `fut` to completion while polling the relay, standing in for the
    /// chipset device unit that would otherwise drive it.
    async fn drive<T>(&self, fut: impl Future<Output = T>) -> T {
        let mut fut = pin!(fut);
        poll_fn(|cx| {
            self.relay.lock().poll_device(cx);
            fut.as_mut().poll(cx)
        })
        .await
    }

    /// The offsets written to the host device so far, in order.
    fn written_offsets(&self) -> Vec<u16> {
        self.cfg.lock().writes.iter().map(|&(o, _)| o).collect()
    }
}

/// A command register value with MMIO enabled or disabled, ready to write to
/// the low word of the status/command DWORD.
fn command(mmio_enabled: bool) -> u32 {
    Command::new().with_mmio_enabled(mmio_enabled).into_bits() as u32
}

/// Enabling MMIO starts an attestation that takes several round trips, so a
/// write arriving in the meantime has to wait rather than trample the operation
/// already in flight.
#[async_test]
async fn write_during_tdisp_operation_waits_its_turn(driver: DefaultDriver) {
    let relay = connect_relay(&driver, true).await;

    let activate = relay.deferred_write(
        HeaderType00::STATUS_COMMAND.0,
        command(true),
        PciConfigByteEnable::LOW_WORD,
    );

    // An unrelated write that would normally pass straight through to the
    // device. It must defer instead, and must not reach the device yet.
    let scratch = relay.deferred_write(SCRATCH_OFFSET, 0xabcd_0000, PciConfigByteEnable::FULL);
    assert!(!relay.written_offsets().contains(&SCRATCH_OFFSET));

    relay.drive(activate.write_future()).await.unwrap();
    relay.drive(scratch.write_future()).await.unwrap();

    // Attestation ran to completion and enabled the device...
    assert_eq!(relay.device.tdisp().tdi_state().await, TdispTdiState::Run);
    // ...and only then did the queued write land.
    let offsets = relay.written_offsets();
    let command_index = offsets
        .iter()
        .position(|&o| o == HeaderType00::STATUS_COMMAND.0)
        .expect("activation writes the command register");
    let scratch_index = offsets
        .iter()
        .position(|&o| o == SCRATCH_OFFSET)
        .expect("the queued write reaches the device");
    assert!(
        command_index < scratch_index,
        "queued write reached the device before the operation finished: {offsets:#x?}"
    );
    assert_eq!(relay.read(SCRATCH_OFFSET), 0xabcd_0000);
}

/// Writes that pile up behind an operation are applied in the order they
/// arrived, and each caller sees its own write complete.
#[async_test]
async fn queued_writes_are_applied_in_arrival_order(driver: DefaultDriver) {
    let relay = connect_relay(&driver, true).await;

    let activate = relay.deferred_write(
        HeaderType00::STATUS_COMMAND.0,
        command(true),
        PciConfigByteEnable::LOW_WORD,
    );
    let first = relay.deferred_write(SCRATCH_OFFSET, 0x1111_0000, PciConfigByteEnable::FULL);
    let second = relay.deferred_write(SCRATCH_OFFSET, 0x2222_0000, PciConfigByteEnable::FULL);
    let third = relay.deferred_write(SCRATCH_OFFSET, 0x3333_0000, PciConfigByteEnable::FULL);

    relay.drive(activate.write_future()).await.unwrap();
    relay.drive(first.write_future()).await.unwrap();
    relay.drive(second.write_future()).await.unwrap();
    relay.drive(third.write_future()).await.unwrap();

    let scratch_writes: Vec<u32> = relay
        .cfg
        .lock()
        .writes
        .iter()
        .filter(|&&(o, _)| o == SCRATCH_OFFSET)
        .map(|&(_, v)| v)
        .collect();
    assert_eq!(scratch_writes, vec![0x1111_0000, 0x2222_0000, 0x3333_0000]);
}

/// A queued write can itself cross an MMIO-enable edge. It has to start a fresh
/// operation when its turn comes rather than be applied on top of the one that
/// was already running.
#[async_test]
async fn queued_write_starts_the_next_tdisp_operation(driver: DefaultDriver) {
    let relay = connect_relay(&driver, true).await;

    let activate = relay.deferred_write(
        HeaderType00::STATUS_COMMAND.0,
        command(true),
        PciConfigByteEnable::LOW_WORD,
    );
    let deactivate = relay.deferred_write(
        HeaderType00::STATUS_COMMAND.0,
        command(false),
        PciConfigByteEnable::LOW_WORD,
    );

    relay.drive(activate.write_future()).await.unwrap();
    relay.drive(deactivate.write_future()).await.unwrap();

    // The second write saw the state the first one left behind, recognized the
    // MMIO-disable edge, and unbound the TDI.
    assert_eq!(
        relay.device.tdisp().tdi_state().await,
        TdispTdiState::Unlocked
    );
    assert_eq!(
        relay.read(HeaderType00::STATUS_COMMAND.0) & command(true),
        0
    );
}

/// Reads are not part of the TDISP flow and keep being answered on the spot.
#[async_test]
async fn reads_are_not_held_off(driver: DefaultDriver) {
    let relay = connect_relay(&driver, true).await;

    let activate = relay.deferred_write(
        HeaderType00::STATUS_COMMAND.0,
        command(true),
        PciConfigByteEnable::LOW_WORD,
    );

    // Still reads the pre-activation command register, synchronously.
    assert_eq!(
        relay.read(HeaderType00::STATUS_COMMAND.0) & command(true),
        0
    );

    relay.drive(activate.write_future()).await.unwrap();
}

/// With nothing in flight, a write that needs no TDISP work is passed straight
/// through, as it was before writes could queue.
#[async_test]
async fn writes_pass_through_when_nothing_is_in_flight(driver: DefaultDriver) {
    let relay = connect_relay(&driver, true).await;

    relay
        .write(SCRATCH_OFFSET, 0x5555_0000, PciConfigByteEnable::FULL)
        .unwrap();
    assert_eq!(relay.read(SCRATCH_OFFSET), 0x5555_0000);

    // A device the host did not offer as TDISP capable never defers at all.
    let plain = connect_relay(&driver, false).await;
    plain
        .write(
            HeaderType00::STATUS_COMMAND.0,
            command(true),
            PciConfigByteEnable::LOW_WORD,
        )
        .unwrap();
}
