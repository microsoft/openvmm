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
use chipset_device::poll_device::PollDevice;
use closeable_mutex::CloseableMutex;
use device_emulators::ReadWriteRequestType;
use device_emulators::read_as_u32_chunks;
use device_emulators::write_as_u32_chunks;
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
    /// Deferred config space writes waiting for inner tokens to complete.
    #[inspect(skip)]
    pending_writes: Vec<PendingDeferredWrite>,
    /// Waker registered by the chipset's poll loop. Used to re-schedule
    /// polling when a new pending write is added from [`MmioIntercept::mmio_write`].
    /// Initialized to a noop waker; replaced on the first [`PollDevice::poll_device`] call.
    #[inspect(skip)]
    waker: Waker,
}

struct PendingDeferredWrite {
    /// Inner tokens from `pci_cfg_write` calls that returned [`IoResult::Defer`],
    /// completed one at a time in order.
    inner_tokens: VecDeque<DeferredToken>,
    /// Outer deferred write to complete once all inner tokens are ready.
    outer_deferred: Option<DeferredWrite>,
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
            pending_writes: Vec::new(),
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
        self.pending_writes = std::mem::take(&mut self.pending_writes)
            .into_iter()
            .filter_map(|mut pending| {
                // Drain tokens one at a time in order; stop at the first still-pending one.
                while let Some(token) = pending.inner_tokens.front_mut() {
                    match token.poll_write(cx) {
                        Poll::Pending => return Some(pending),
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(e)) => {
                            // If any of the inner tokens error, error the entire deferred write and drop any remaining tokens.
                            if let Some(deferred) = pending.outer_deferred.take() {
                                deferred.complete_error(e);
                            }
                            return None;
                        }
                    }
                    pending.inner_tokens.pop_front();
                }
                if let Some(deferred) = pending.outer_deferred.take() {
                    deferred.complete();
                }
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
                    let deferred = {
                        let mut device = self.device.lock();
                        let pci = device.supports_pci().unwrap();
                        let mut buf = 0;
                        let mut deferred: VecDeque<DeferredToken> = VecDeque::new();
                        write_as_u32_chunks(
                            offset,
                            data,
                            |address, request_type| match request_type {
                                ReadWriteRequestType::Write(value) => {
                                    match pci.pci_cfg_write(address, value) {
                                        IoResult::Ok => {}
                                        IoResult::Err(err) => panic!(
                                            "config space write failed: address={address:#x}, value={value:#x}, error={err:?}"
                                        ),
                                        IoResult::Defer(token) => deferred.push_back(token),
                                    }
                                    None
                                }
                                ReadWriteRequestType::Read => Some(
                                    pci.pci_cfg_read(address, &mut buf)
                                        .now_or_never()
                                        .map(|_| buf)
                                        .unwrap_or(0),
                                ),
                            },
                        );
                        deferred
                    };
                    if !deferred.is_empty() {
                        return self.enqueue_deferred_write(deferred);
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

impl VpciBusDevice {
    /// Stores a pending deferred write and wakes the poll loop to drive it.
    fn enqueue_deferred_write(&mut self, deferred: VecDeque<DeferredToken>) -> IoResult {
        let (outer_deferred, outer_token) = defer_write();
        self.pending_writes.push(PendingDeferredWrite {
            inner_tokens: deferred,
            outer_deferred: Some(outer_deferred),
        });
        self.waker.wake_by_ref();
        IoResult::Defer(outer_token)
    }

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
        let device = Arc::new(CloseableMutex::new(DeferWriteDevice::new()));

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
    }
}
