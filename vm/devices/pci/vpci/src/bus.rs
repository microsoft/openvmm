// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VPCI bus implementation.

use crate::device::NotPciDevice;
use crate::device::VpciChannel;
use crate::device::VpciConfigSpace;
use crate::device::VpciConfigSpaceOffset;
use crate::device::VpciConfigSpaceVtom;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::io::deferred::DeferredToken;
use chipset_device::io::deferred::DeferredWrite;
use chipset_device::io::deferred::defer_write;
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::PciConfigSpace;
use chipset_device::poll_device::PollDevice;
use closeable_mutex::CloseableMutex;
use device_emulators::read_as_u32_chunks;
use guid::Guid;
use hvdef::HV_PAGE_SIZE;
use inspect::InspectMut;
use std::collections::VecDeque;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;
use thiserror::Error;
use vmbus_channel::simple::SimpleDeviceHandle;
use vmbus_channel::simple::offer_simple_device;
use vmcore::device_state::ChangeDeviceState;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::RestoreError;
use vmcore::save_restore::SaveError;
use vmcore::save_restore::SaveRestore;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vpci_protocol as protocol;
use vpci_protocol::SlotNumber;

/// A VPCI bus, which can be used to enumerate PCI devices to a guest over
/// vmbus.
///
/// Note that this implementation only allows a single device per bus currently.
/// In practice, this is the only used and well-tested configuration in Hyper-V.
#[derive(InspectMut)]
pub struct VpciBus {
    #[inspect(mut, flatten)]
    bus_device: VpciBusDevice,
    #[inspect(flatten)]
    channel: SimpleDeviceHandle<VpciChannel>,
}

/// The chipset device portion of the VPCI bus.
///
/// This is primarily used for testing. You should use [`VpciBus`] in
/// product code to get a single device/state unit.
#[derive(InspectMut)]
pub struct VpciBusDevice {
    #[inspect(skip)]
    device: Arc<CloseableMutex<dyn ChipsetDevice>>,
    config_space_offset: VpciConfigSpaceOffset,
    #[inspect(with = "|&x| u32::from(x)")]
    current_slot: SlotNumber,
    /// Track vtom as when isolated with vtom enabled, guests may access mmio
    /// with or without vtom set.
    vtom: Option<u64>,
    /// Deferred config space writes being driven through the state machine.
    #[inspect(skip)]
    pending_actions: Vec<PendingConfigWrite>,
    /// Waker registered by the chipset's poll loop. Used to re-schedule
    /// polling when a new pending write is added from [`MmioIntercept::mmio_write`].
    /// Initialized to a noop waker; replaced on the first [`PollDevice::poll_device`] call.
    #[inspect(skip)]
    waker: Waker,
}

/// State for a config space write that could not complete synchronously.
///
/// Drives writes one at a time: when `device_write` resolves, the next entry
/// from `remaining` is issued. The bus deferred token `bus_write` is completed
/// once all entries finish (or errored if any entry fails).
struct PendingConfigWrite {
    /// Token for the currently in-flight `pci_cfg_write` call.
    device_write: DeferredToken,
    deferred_address: u16,
    deferred_value: u32,

    /// Outer write completed once every entry in `remaining` has finished.
    bus_write: DeferredWrite,
    /// Remaining `(config_offset, value)` pairs to write, in order.
    remaining: VecDeque<(u16, u32)>,
}

/// An error creating a VPCI bus.
#[derive(Debug, Error)]
pub enum CreateBusError {
    /// The device is not a PCI device.
    #[error(transparent)]
    NotPci(NotPciDevice),
    /// The vmbus channel offer failed.
    #[error("failed to offer vpci vmbus channel")]
    Offer(#[source] anyhow::Error),
}

impl VpciBusDevice {
    /// Returns a new VPCI bus device, along with the vmbus channel used for bus
    /// communications.
    pub fn new(
        instance_id: Guid,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        register_mmio: &mut dyn RegisterMmioIntercept,
        msi_controller: VpciInterruptMapper,
        vtom: Option<u64>,
    ) -> Result<(Self, VpciChannel), NotPciDevice> {
        let config_space = VpciConfigSpace::new(
            register_mmio.new_io_region(&format!("vpci-{instance_id}-config"), 2 * HV_PAGE_SIZE),
            vtom.map(|vtom| VpciConfigSpaceVtom {
                vtom,
                control_mmio: register_mmio
                    .new_io_region(&format!("vpci-{instance_id}-config-vtom"), 2 * HV_PAGE_SIZE),
            }),
        );
        let config_space_offset = config_space.offset().clone();
        let channel = VpciChannel::new(&device, instance_id, config_space, msi_controller)?;

        let this = Self {
            device,
            config_space_offset,
            current_slot: SlotNumber::from(0),
            vtom,
            pending_actions: Vec::new(),
            waker: Waker::noop().clone(),
        };

        Ok((this, channel))
    }

    #[cfg(test)]
    pub(crate) fn config_space_offset(&self) -> &VpciConfigSpaceOffset {
        &self.config_space_offset
    }
}

impl VpciBus {
    /// Creates a new VPCI bus.
    pub async fn new(
        driver_source: &VmTaskDriverSource,
        instance_id: Guid,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        register_mmio: &mut dyn RegisterMmioIntercept,
        vmbus: &dyn vmbus_channel::bus::ParentBus,
        msi_controller: VpciInterruptMapper,
        vtom: Option<u64>,
    ) -> Result<Self, CreateBusError> {
        let (bus, channel) = VpciBusDevice::new(
            instance_id,
            device.clone(),
            register_mmio,
            msi_controller.clone(),
            vtom,
        )
        .map_err(CreateBusError::NotPci)?;
        let channel = offer_simple_device(driver_source, vmbus, channel)
            .await
            .map_err(CreateBusError::Offer)?;

        Ok(Self {
            bus_device: bus,
            channel,
        })
    }
}

impl ChangeDeviceState for VpciBus {
    fn start(&mut self) {
        self.channel.start();
    }

    async fn stop(&mut self) {
        self.channel.stop().await;
    }

    async fn reset(&mut self) {
        self.channel.reset().await;
    }
}

impl SaveRestore for VpciBus {
    // TODO: support saved state
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        Ok(NoSavedState)
    }

    fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
        Ok(())
    }
}

impl ChipsetDevice for VpciBus {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        self.bus_device.supports_mmio()
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        self.bus_device.supports_poll_device()
    }
}

impl ChipsetDevice for VpciBusDevice {
    fn supports_mmio(&mut self) -> Option<&mut dyn MmioIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

impl PollDevice for VpciBusDevice {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        self.waker = cx.waker().clone();
        self.pending_actions = std::mem::take(&mut self.pending_actions)
            .into_iter()
            .filter_map(|mut action| {
                // If the current write is still pending, poll it for completion.
                match action.device_write.poll_write(cx) {
                    Poll::Pending => return Some(action),
                    Poll::Ready(Err(e)) => {
                        panic!(
                            "deferred config space write failed: address={:#x}, value={:#x}, error={e:?}", 
                            action.deferred_address,
                            action.deferred_value);
                    }
                    Poll::Ready(Ok(())) => {}
                }

                // The current write completed. Issue the next writes until
                // another deferral or exhaustion. Complete non-deferred writes immediately
                // in this loop to avoid unnecessary context switches.
                let mut device = self.device.lock();
                let pci = device.supports_pci().unwrap();
                while let Some((address, value)) = action.remaining.pop_front() {
                    match pci.pci_cfg_write(address, value) {
                        IoResult::Ok => {} // continue to next write
                        IoResult::Err(e) => {
                            panic!(
                                "config space write failed: address={address:#x}, value={value:#x}, error={e:?}"
                            );
                        }
                        IoResult::Defer(token) => {
                            action.device_write = token;
                            action.deferred_address = address;
                            action.deferred_value = value;
                            return Some(action);
                        }
                    }
                }

                // If there are no more writes to issue, complete the outer token and finish.
                action.bus_write.complete();
                None
            })
            .collect();
    }
}

impl MmioIntercept for VpciBusDevice {
    fn mmio_read(&mut self, addr: u64, data: &mut [u8]) -> IoResult {
        tracing::trace!(addr, "VPCI bus MMIO read");

        // Remove vtom, as the guest may access it with or without set.
        let addr = addr & !self.vtom.unwrap_or(0);

        let reg = match self.register(addr, data.len()) {
            Ok(reg) => reg,
            Err(err) => return IoResult::Err(err),
        };
        match reg {
            Register::SlotNumber => return IoResult::Err(IoError::InvalidRegister),
            Register::ConfigSpace(offset) => {
                // FUTURE: support a bus with multiple devices.
                if u32::from(self.current_slot) == 0 {
                    let mut device = self.device.lock();
                    let pci = device.supports_pci().unwrap();
                    let mut buf = 0;
                    read_as_u32_chunks(offset, data, |addr| {
                        pci.pci_cfg_read(addr, &mut buf)
                            .now_or_never()
                            .map(|_| buf)
                            .unwrap_or(0)
                    });
                } else {
                    tracelimit::warn_ratelimited!(slot = ?self.current_slot, offset, "no device at slot for config space read");
                    data.fill(!0);
                }
            }
        }
        IoResult::Ok
    }

    fn mmio_write(&mut self, addr: u64, data: &[u8]) -> IoResult {
        tracing::trace!(addr, "VPCI bus MMIO write");

        // Remove vtom, as the guest may access it with or without set.
        let addr = addr & !self.vtom.unwrap_or(0);

        let reg = match self.register(addr, data.len()) {
            Ok(reg) => reg,
            Err(err) => return IoResult::Err(err),
        };
        match reg {
            Register::SlotNumber => {
                let Ok(data) = data.try_into().map(u32::from_ne_bytes) else {
                    return IoResult::Err(IoError::InvalidAccessSize);
                };
                self.current_slot = SlotNumber::from(data);
            }
            Register::ConfigSpace(offset) => {
                // FUTURE: support a bus with multiple devices.
                if u32::from(self.current_slot) == 0 {
                    let mut device = self.device.lock();
                    let pci = device.supports_pci().unwrap();

                    // Pre-compute all u32 writes (reads done synchronously).
                    let mut writes = compute_config_writes(pci, offset, data);

                    // Issue writes one at a time; defer on the first that needs it.
                    while let Some((address, value)) = writes.pop_front() {
                        match pci.pci_cfg_write(address, value) {
                            IoResult::Ok => {}
                            IoResult::Err(err) => panic!(
                                "config space write failed: address={address:#x}, value={value:#x}, error={err:?}"
                            ),
                            IoResult::Defer(device_write) => {
                                drop(device);
                                let (bus_write, bus_token) = defer_write();
                                self.pending_actions.push(PendingConfigWrite {
                                    device_write,
                                    deferred_address: address,
                                    deferred_value: value,
                                    bus_write,
                                    remaining: writes,
                                });
                                self.waker.wake_by_ref();
                                return IoResult::Defer(bus_token);
                            }
                        }
                    }
                } else {
                    tracelimit::warn_ratelimited!(slot = ?self.current_slot, offset, "no device at slot for config space write");
                }
            }
        }
        IoResult::Ok
    }
}

enum Register {
    SlotNumber,
    ConfigSpace(u16),
}

/// Pre-computes the sequence of aligned u32 writes needed for a config space
/// write, performing any required read-modify-write reads synchronously.
///
/// The returned queue contains `(config_offset, value)` pairs in order;
/// callers issue them one at a time and handle deferred completions.
///
/// Mimics behavior of [`device_emulators::write_as_u32_chunks`]
fn compute_config_writes(
    pci: &mut dyn PciConfigSpace,
    offset: u16,
    data: &[u8],
) -> VecDeque<(u16, u32)> {
    let mut writes = VecDeque::new();
    let mut next_offset = offset as u64;

    // Unaligned start: read-modify-write the leading partial u32.
    let remaining = if next_offset & 3 != 0 {
        let aligned = (next_offset & !3) as u16;
        let mut existing = 0u32;
        let read_val = pci
            .pci_cfg_read(aligned, &mut existing)
            .now_or_never()
            .map(|_| existing)
            .unwrap_or(0);
        let mut bytes = read_val.to_ne_bytes();
        let u32_offset = (next_offset & 3) as usize;
        let byte_count = (4 - u32_offset).min(data.len());
        bytes[u32_offset..u32_offset + byte_count].copy_from_slice(&data[..byte_count]);
        writes.push_back((aligned, u32::from_ne_bytes(bytes)));
        next_offset += byte_count as u64;
        &data[byte_count..]
    } else {
        data
    };

    // Aligned middle chunks: full u32 writes.
    for chunk in remaining.chunks_exact(4) {
        let val = u32::from_ne_bytes(chunk.try_into().unwrap());
        writes.push_back((next_offset as u16, val));
        next_offset += 4;
    }

    // Unaligned end: read-modify-write the trailing partial u32.
    let extra = remaining.chunks_exact(4).remainder();
    if !extra.is_empty() {
        let mut existing = 0u32;
        let read_val = pci
            .pci_cfg_read(next_offset as u16, &mut existing)
            .now_or_never()
            .map(|_| existing)
            .unwrap_or(0);
        let mut bytes = read_val.to_ne_bytes();
        bytes[..extra.len()].copy_from_slice(extra);
        writes.push_back((next_offset as u16, u32::from_ne_bytes(bytes)));
    }

    writes
}

impl VpciBusDevice {
    fn register(&self, addr: u64, len: usize) -> Result<Register, IoError> {
        // Note that this base address might be concurrently changing. We can
        // ignore accesses that are to addresses that don't make sense.
        let config_base = self
            .config_space_offset
            .get()
            .ok_or(IoError::InvalidRegister)?;

        let offset = addr.wrapping_sub(config_base);
        let page = offset & protocol::MMIO_PAGE_MASK;
        let offset_in_page = (offset & !protocol::MMIO_PAGE_MASK) as u16;

        // Accesses cannot straddle a page boundary.
        if (offset_in_page as u64 + len as u64) & protocol::MMIO_PAGE_MASK != 0 {
            return Err(IoError::InvalidAccessSize);
        }

        let reg = match page {
            protocol::MMIO_PAGE_SLOT_NUMBER => {
                // Only a 32-bit access at the beginning of the page is allowed.
                if offset_in_page != 0 {
                    return Err(IoError::InvalidRegister);
                }
                if len != 4 {
                    return Err(IoError::InvalidAccessSize);
                }
                Register::SlotNumber
            }
            protocol::MMIO_PAGE_CONFIG_SPACE => Register::ConfigSpace(offset_in_page),
            _ => return Err(IoError::InvalidRegister),
        };

        Ok(reg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestVpciInterruptController;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::io::deferred::DeferredWrite;
    use chipset_device::io::deferred::defer_write;
    use chipset_device::mmio::ExternallyManagedMmioIntercepts;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device::pci::PciConfigSpace;
    use chipset_device::poll_device::PollDevice;
    use closeable_mutex::CloseableMutex;
    use guid::Guid;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use vmcore::vpci_msi::VpciInterruptMapper;

    /// A minimal PCI device that returns `IoResult::Ok` for all operations
    /// until `start_deferring` is called, after which `pci_cfg_write` defers
    /// completion until driven by `poll_device`.
    struct DeferWriteDevice {
        pending_write: Option<DeferredWrite>,
        defer_writes: bool,
    }

    impl DeferWriteDevice {
        fn new() -> Self {
            Self {
                pending_write: None,
                defer_writes: false,
            }
        }

        fn start_deferring(&mut self) {
            self.defer_writes = true;
        }
    }

    impl InspectMut for DeferWriteDevice {
        fn inspect_mut(&mut self, req: inspect::Request<'_>) {
            req.ignore();
        }
    }

    impl ChipsetDevice for DeferWriteDevice {
        fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
            Some(self)
        }

        fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
            Some(self)
        }
    }

    impl PollDevice for DeferWriteDevice {
        fn poll_device(&mut self, _cx: &mut Context<'_>) {
            if let Some(deferred) = self.pending_write.take() {
                deferred.complete();
            }
        }
    }

    impl PciConfigSpace for DeferWriteDevice {
        fn pci_cfg_read(&mut self, _offset: u16, _value: &mut u32) -> IoResult {
            IoResult::Ok
        }

        fn pci_cfg_write(&mut self, _offset: u16, _value: u32) -> IoResult {
            if self.defer_writes {
                assert!(
                    self.pending_write.is_none(),
                    "new write issued before previous deferred write completed"
                );
                let (deferred, token) = defer_write();
                self.pending_write = Some(deferred);
                IoResult::Defer(token)
            } else {
                IoResult::Ok
            }
        }
    }

    /// Verifies that `VpciBusDevice` correctly suspends a VP on a deferred
    /// `pci_cfg_write` and completes it once `poll_device` drives the inner
    /// token to completion.
    #[async_test]
    async fn verify_deferred_pci_cfg_write_via_bus(driver: DefaultDriver) {
        const BASE_ADDR: u64 = 0x1000_0000;
        const OFFSET_CMD_REG: u64 = 4;

        let msi_controller = TestVpciInterruptController::new();
        let device: Arc<CloseableMutex<DeferWriteDevice>> =
            Arc::new(CloseableMutex::new(DeferWriteDevice::new()));

        let (bus, _channel) = VpciBusDevice::new(
            Guid::new_random(),
            device.clone(),
            &mut ExternallyManagedMmioIntercepts,
            VpciInterruptMapper::new(msi_controller),
            None,
        )
        .unwrap();

        let bus = Arc::new(CloseableMutex::new(bus));

        // Set the MMIO base so that the address decoding in mmio_write works.
        bus.lock().config_space_offset().set(BASE_ADDR);

        // Check that writes are Ok and not deferred before `start_deferring`.
        let write_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + OFFSET_CMD_REG;
        let result = bus
            .lock()
            .mmio_write(write_addr, &0xdeadbeefu32.to_ne_bytes());
        assert!(matches!(result, IoResult::Ok));

        // Enable write deferral on the inner device now that probing is done.
        device.lock().start_deferring();

        // Write to config space offset 4 (command register) via the MMIO
        // interface. This should be deferred because the inner device
        // (DeferWriteDevice) now defers the IoResult from pci_cfg_write.
        let write_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + OFFSET_CMD_REG;
        let result = bus
            .lock()
            .mmio_write(write_addr, &0xdeadbeefu32.to_ne_bytes());
        assert!(matches!(result, IoResult::Defer(_)));

        // Spawn a task that drives poll_device to simulate the chipset state unit.
        let bus_clone = bus.clone();
        let device_clone = device.clone();
        let poll_ran = Arc::new(AtomicBool::new(false));
        let poll_ran_clone = poll_ran.clone();
        driver
            .spawn("poll-device", async move {
                std::future::poll_fn(|cx| {
                    // First call: registers the real waker on the inner token.
                    bus_clone.lock().poll_device(cx);
                    // Complete the inner write via the device's poll_device.
                    device_clone.lock().poll_device(cx);
                    // Second call: inner token is now ready; completes the outer token.
                    bus_clone.lock().poll_device(cx);

                    poll_ran_clone.store(true, Ordering::SeqCst);
                    Poll::Ready(())
                })
                .await;
            })
            .detach();

        // Await the outer deferred token; unblocked once poll_device completes it.
        if let IoResult::Defer(token) = result {
            token
                .write_future()
                .await
                .expect("deferred PCI config write should complete successfully");
        }

        assert!(
            poll_ran.load(Ordering::SeqCst),
            "poll_device task did not run before the deferred write completed"
        );

        // --- Part 2: multiple u32 writes are each deferred and polled in order ---
        //
        // Write 12 bytes (3 contiguous u32s) at a 4-byte-aligned offset. Each
        // pci_cfg_write is deferred by the device. Verify that:
        //   - the bus returns a single outer Defer for the whole MMIO write,
        //   - writes are issued strictly one at a time (enforced by the assert in
        //     DeferWriteDevice::pci_cfg_write), and
        //   - exactly 3 rounds of (bus poll → device complete → bus poll) are
        //     needed to drive the outer Defer to completion.
        const MULTI_OFFSET: u64 = 8;
        let multi_write_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + MULTI_OFFSET;
        let multi_result = bus.lock().mmio_write(multi_write_addr, &[0xAAu8; 12]);
        assert!(
            matches!(multi_result, IoResult::Defer(_)),
            "3-u32 write should return a single outer Defer"
        );

        // After mmio_write returns, only the first pci_cfg_write has been issued.
        assert!(
            device.lock().pending_write.is_some(),
            "write 1/3 should be in flight immediately after mmio_write"
        );

        let bus_clone2 = bus.clone();
        let device_clone2 = device.clone();
        let rounds = Arc::new(AtomicUsize::new(0));
        let rounds_clone = rounds.clone();
        driver
            .spawn("poll-device-multi", async move {
                std::future::poll_fn(|cx| {
                    // Drive one round per deferred write. Each round:
                    //   1. bus poll_device: sees current write pending, registers waker
                    //   2. device poll_device: completes the current write
                    //   3. bus poll_device: picks up completion and issues the next
                    //      write (or completes the outer token on the last one)
                    for _ in 0..3 {
                        bus_clone2.lock().poll_device(cx);
                        device_clone2.lock().poll_device(cx);
                        bus_clone2.lock().poll_device(cx);
                        rounds_clone.fetch_add(1, Ordering::SeqCst);
                    }
                    Poll::Ready(())
                })
                .await;
            })
            .detach();

        if let IoResult::Defer(token) = multi_result {
            token
                .write_future()
                .await
                .expect("multi-u32 deferred write should complete successfully");
        }

        assert_eq!(
            rounds.load(Ordering::SeqCst),
            3,
            "each of the 3 deferred writes should have required an independent poll round"
        );
    }
}
