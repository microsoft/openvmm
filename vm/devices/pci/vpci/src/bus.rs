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
use chipset_device::mmio::MmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pci::ByteEnabledDwordRead;
use chipset_device::pci::ByteEnabledDwordWrite;
use chipset_device::pci::PciConfigAddress;
use chipset_device::pci::PciConfigByteEnable;
use chipset_device::poll_device::PollDevice;
use closeable_mutex::CloseableMutex;
use guid::Guid;
use hvdef::HV_PAGE_SIZE;
use inspect::InspectMut;
use pci_core::bus_cfg::PciBusCfgAccessCallbacks;
use pci_core::bus_cfg::PciBusCfgAccessHandler;
use std::sync::Arc;
use std::task::Context;
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
    /// Bus config space accesses handler.
    bus_cfg_handler: PciBusCfgAccessHandler,
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

/// Configuration for a VPCI bus instance.
pub struct VpciBusConfig {
    /// The VPCI device instance ID.
    pub instance_id: Guid,
    /// VTOM value for isolated VMs, if applicable.
    pub vtom: Option<u64>,
    /// NUMA node affinity reported to the guest.
    pub vnode: Option<u16>,
}

impl VpciBusDevice {
    /// Returns a new VPCI bus device, along with the vmbus channel used for bus
    /// communications.
    pub fn new(
        config: VpciBusConfig,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        register_mmio: &mut dyn RegisterMmioIntercept,
        msi_controller: VpciInterruptMapper,
    ) -> Result<(Self, VpciChannel), NotPciDevice> {
        let instance_id = config.instance_id;
        let config_space = VpciConfigSpace::new(
            register_mmio.new_io_region(&format!("vpci-{instance_id}-config"), 2 * HV_PAGE_SIZE),
            config.vtom.map(|vtom| VpciConfigSpaceVtom {
                vtom,
                control_mmio: register_mmio
                    .new_io_region(&format!("vpci-{instance_id}-config-vtom"), 2 * HV_PAGE_SIZE),
            }),
        );
        let config_space_offset = config_space.offset().clone();
        let channel = VpciChannel::new(
            &device,
            instance_id,
            config_space,
            msi_controller,
            config.vnode,
        )?;

        let this = Self {
            device,
            config_space_offset,
            current_slot: SlotNumber::from(0),
            vtom: config.vtom,
            bus_cfg_handler: PciBusCfgAccessHandler::new(),
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
        config: VpciBusConfig,
        device: Arc<CloseableMutex<dyn ChipsetDevice>>,
        register_mmio: &mut dyn RegisterMmioIntercept,
        vmbus: &dyn vmbus_channel::bus::ParentBus,
        msi_controller: VpciInterruptMapper,
    ) -> Result<Self, CreateBusError> {
        let (bus, channel) = VpciBusDevice::new(
            config,
            device.clone(),
            register_mmio,
            msi_controller.clone(),
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
        let mut callback = PciBusCfgAccessCallbackView::new(&mut self.device);
        self.bus_cfg_handler.poll(cx, &mut callback);
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
            Register::ConfigSpace(address, byte_enable) => {
                // FUTURE: support a bus with multiple devices.
                if u32::from(self.current_slot) == 0 {
                    let mut value_u32 = 0;
                    let mut value = ByteEnabledDwordRead::new(&mut value_u32, byte_enable);

                    let mut callback = PciBusCfgAccessCallbackView::new(&mut self.device);
                    let result =
                        self.bus_cfg_handler
                            .read(address, value.reborrow(), &mut callback);

                    if matches!(result, IoResult::Ok) {
                        value.fill_intercept_buffer(data);
                    }

                    return result;
                } else {
                    tracelimit::warn_ratelimited!(slot = ?self.current_slot, offset = address.byte_offset(), "no device at slot for config space read");
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
            Register::ConfigSpace(address, byte_enable) => {
                // FUTURE: support a bus with multiple devices.
                if u32::from(self.current_slot) == 0 {
                    let value = ByteEnabledDwordWrite::from_intercept_buffer(byte_enable, data);
                    let mut callback = PciBusCfgAccessCallbackView::new(&mut self.device);
                    return self.bus_cfg_handler.write(address, value, &mut callback);
                } else {
                    tracelimit::warn_ratelimited!(slot = ?self.current_slot, offset = address.byte_offset(), "no device at slot for config space write");
                }
            }
        }
        IoResult::Ok
    }
}

enum Register {
    SlotNumber,
    ConfigSpace(PciConfigAddress, PciConfigByteEnable),
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
            protocol::MMIO_PAGE_CONFIG_SPACE => {
                let address = PciConfigAddress::new(0, 0, offset_in_page / 4)
                    .ok_or(IoError::InvalidRegister)?;
                let byte_enable = PciConfigByteEnable::from_offset_len(offset_in_page, len)?;
                Register::ConfigSpace(address, byte_enable)
            }
            _ => return Err(IoError::InvalidRegister),
        };

        Ok(reg)
    }
}

struct PciBusCfgAccessCallbackView<'a> {
    device: &'a mut Arc<CloseableMutex<dyn ChipsetDevice>>,
}

impl<'a> PciBusCfgAccessCallbackView<'a> {
    fn new(device: &'a mut Arc<CloseableMutex<dyn ChipsetDevice>>) -> Self {
        Self { device }
    }
}

impl<'a> PciBusCfgAccessCallbacks for PciBusCfgAccessCallbackView<'a> {
    fn read(&mut self, addr: PciConfigAddress, value: &mut u32) -> IoResult {
        self.device
            .lock()
            .supports_pci()
            .unwrap()
            .pci_cfg_read(addr.byte_offset(), value)
    }

    fn write(&mut self, addr: PciConfigAddress, value: u32) -> IoResult {
        self.device
            .lock()
            .supports_pci()
            .unwrap()
            .pci_cfg_write(addr.byte_offset(), value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestVpciInterruptController;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::io::deferred::DeferredRead;
    use chipset_device::io::deferred::DeferredWrite;
    use chipset_device::io::deferred::defer_read;
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
    use std::task::Context;
    use std::task::Poll;
    use vmcore::vpci_msi::VpciInterruptMapper;

    const BASE_ADDR: u64 = 0x1000_0000;

    /// A minimal PCI device that returns `IoResult::Ok` for all operations
    /// until `start_deferring` is called, after which `pci_cfg_write` defers
    /// completion until driven by `poll_device`.
    struct DeferWriteDevice {
        pending_read: Option<DeferredRead>,
        pending_write: Option<DeferredWrite>,
        defer_reads: bool,
        defer_writes: bool,
        read_error: Option<IoError>,
        write_error: Option<IoError>,
        read_value: u32,
        writes: Vec<(u16, u32)>,
    }

    impl DeferWriteDevice {
        fn new() -> Self {
            Self {
                pending_read: None,
                pending_write: None,
                defer_reads: false,
                defer_writes: false,
                read_error: None,
                write_error: None,
                read_value: 0,
                writes: Vec::new(),
            }
        }

        fn start_deferring_reads(&mut self, value: u32) {
            self.read_value = value;
            self.defer_reads = true;
        }

        fn start_deferring(&mut self) {
            self.defer_writes = true;
        }

        fn fail_deferred_reads(&mut self, error: IoError) {
            self.read_error = Some(error);
            self.defer_reads = true;
        }

        fn fail_deferred_writes(&mut self, error: IoError) {
            self.write_error = Some(error);
            self.defer_writes = true;
        }
    }

    struct VpciTestRig {
        bus: Arc<CloseableMutex<VpciBusDevice>>,
        device: Arc<CloseableMutex<DeferWriteDevice>>,
    }

    impl VpciTestRig {
        fn new() -> Self {
            let msi_controller = TestVpciInterruptController::new();
            let device: Arc<CloseableMutex<DeferWriteDevice>> =
                Arc::new(CloseableMutex::new(DeferWriteDevice::new()));

            let (bus, _channel) = VpciBusDevice::new(
                VpciBusConfig {
                    instance_id: Guid::new_random(),
                    vtom: None,
                    vnode: None,
                },
                device.clone(),
                &mut ExternallyManagedMmioIntercepts,
                VpciInterruptMapper::new(msi_controller),
            )
            .unwrap();

            let bus = Arc::new(CloseableMutex::new(bus));
            bus.lock().config_space_offset().set(BASE_ADDR);

            Self { bus, device }
        }

        fn config_addr(offset: u64) -> u64 {
            BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + offset
        }

        fn slot_addr() -> u64 {
            BASE_ADDR + protocol::MMIO_PAGE_SLOT_NUMBER
        }

        fn poll_bus_and_device(&self) {
            let mut cx = Context::from_waker(std::task::Waker::noop());
            self.bus.lock().poll_device(&mut cx);
            self.device.lock().poll_device(&mut cx);
            self.bus.lock().poll_device(&mut cx);
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
            if let Some(deferred) = self.pending_read.take() {
                if let Some(error) = self.read_error.take() {
                    deferred.complete_error(error);
                } else {
                    deferred.complete(&self.read_value.to_ne_bytes());
                }
            }
            if let Some(deferred) = self.pending_write.take() {
                if let Some(error) = self.write_error.take() {
                    deferred.complete_error(error);
                } else {
                    deferred.complete();
                }
            }
        }
    }

    impl PciConfigSpace for DeferWriteDevice {
        fn pci_cfg_read(&mut self, _offset: u16, value: &mut u32) -> IoResult {
            if self.defer_reads {
                assert!(
                    self.pending_read.is_none(),
                    "new read issued before previous deferred read completed"
                );
                let (deferred, token) = defer_read();
                self.pending_read = Some(deferred);
                IoResult::Defer(token)
            } else {
                *value = self.read_value;
                IoResult::Ok
            }
        }

        fn pci_cfg_write(&mut self, offset: u16, value: u32) -> IoResult {
            self.writes.push((offset, value));
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
            VpciBusConfig {
                instance_id: Guid::new_random(),
                vtom: None,
                vnode: None,
            },
            device.clone(),
            &mut ExternallyManagedMmioIntercepts,
            VpciInterruptMapper::new(msi_controller),
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

        // A PCI config access cannot span multiple DWORDs. Reject it before
        // sending anything to the device, even if the device would defer.
        const MULTI_OFFSET: u64 = 8;
        let multi_write_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + MULTI_OFFSET;
        let writes_before = device.lock().writes.len();
        let multi_result = bus.lock().mmio_write(multi_write_addr, &[0xaa; 12]);
        assert!(
            matches!(multi_result, IoResult::Err(IoError::InvalidAccessSize)),
            "multi-DWORD write should be rejected"
        );
        assert!(
            device.lock().pending_write.is_none(),
            "rejected write should not issue a device write"
        );
        assert_eq!(
            device.lock().writes.len(),
            writes_before,
            "rejected write should not be recorded by the device"
        );
    }

    #[async_test]
    async fn verify_deferred_pci_cfg_read_and_read_for_write_via_bus(_driver: DefaultDriver) {
        const BASE_ADDR: u64 = 0x1000_0000;
        const READ_OFFSET: u64 = 4;
        const WRITE_OFFSET: u64 = 5;

        let msi_controller = TestVpciInterruptController::new();
        let device: Arc<CloseableMutex<DeferWriteDevice>> =
            Arc::new(CloseableMutex::new(DeferWriteDevice::new()));

        let (bus, _channel) = VpciBusDevice::new(
            VpciBusConfig {
                instance_id: Guid::new_random(),
                vtom: None,
                vnode: None,
            },
            device.clone(),
            &mut ExternallyManagedMmioIntercepts,
            VpciInterruptMapper::new(msi_controller),
        )
        .unwrap();

        let bus = Arc::new(CloseableMutex::new(bus));
        bus.lock().config_space_offset().set(BASE_ADDR);

        device.lock().start_deferring_reads(0x5566_7788);
        let read_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + READ_OFFSET;
        let mut read_data = [0; 4];
        let read_result = bus.lock().mmio_read(read_addr, &mut read_data);
        assert!(matches!(read_result, IoResult::Defer(_)));

        std::future::poll_fn(|cx| {
            bus.lock().poll_device(cx);
            device.lock().poll_device(cx);
            bus.lock().poll_device(cx);
            Poll::Ready(())
        })
        .await;

        if let IoResult::Defer(token) = read_result {
            token
                .read_future(&mut read_data)
                .await
                .expect("deferred PCI config read should complete successfully");
        }
        assert_eq!(u32::from_ne_bytes(read_data), 0x5566_7788);

        device.lock().start_deferring_reads(0x1122_3344);
        let write_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + WRITE_OFFSET;
        let write_result = bus.lock().mmio_write(write_addr, &[0xaa]);
        assert!(matches!(write_result, IoResult::Defer(_)));

        std::future::poll_fn(|cx| {
            bus.lock().poll_device(cx);
            device.lock().poll_device(cx);
            bus.lock().poll_device(cx);
            Poll::Ready(())
        })
        .await;

        if let IoResult::Defer(token) = write_result {
            token
                .write_future()
                .await
                .expect("deferred PCI config read-for-write should complete successfully");
        }

        let mut expected = 0x1122_3344u32.to_ne_bytes();
        expected[1] = 0xaa;
        assert_eq!(
            device.lock().writes.pop(),
            Some((4, u32::from_ne_bytes(expected)))
        );

        let mut read_data = [0; 2];
        let read_addr = BASE_ADDR + protocol::MMIO_PAGE_CONFIG_SPACE + 7;
        let read_result = bus.lock().mmio_read(read_addr, &mut read_data);
        assert!(
            matches!(read_result, IoResult::Err(_)),
            "read crossing a DWORD boundary should be rejected"
        );
        assert!(
            device.lock().pending_read.is_none(),
            "rejected read should not issue a device read"
        );
    }

    #[async_test]
    async fn verify_deferred_pci_cfg_errors_via_bus(_driver: DefaultDriver) {
        const READ_OFFSET: u64 = 4;
        const WRITE_OFFSET: u64 = 8;

        let rig = VpciTestRig::new();

        rig.device.lock().fail_deferred_reads(IoError::NoResponse);
        let writes_before = rig.device.lock().writes.len();
        let mut read_data = [0; 4];
        let read_result = rig
            .bus
            .lock()
            .mmio_read(VpciTestRig::config_addr(READ_OFFSET), &mut read_data);
        let IoResult::Defer(read_token) = read_result else {
            panic!("read should defer before completing with an error");
        };

        rig.poll_bus_and_device();
        assert!(matches!(
            read_token.read_future(&mut read_data).await,
            Err(IoError::NoResponse)
        ));
        assert_eq!(rig.device.lock().writes.len(), writes_before);

        rig.device.lock().fail_deferred_writes(IoError::NoResponse);
        let write_result = rig.bus.lock().mmio_write(
            VpciTestRig::config_addr(WRITE_OFFSET),
            &0xaabb_ccddu32.to_ne_bytes(),
        );
        let IoResult::Defer(write_token) = write_result else {
            panic!("write should defer before completing with an error");
        };

        rig.poll_bus_and_device();
        assert!(matches!(
            write_token.write_future().await,
            Err(IoError::NoResponse)
        ));
        assert_eq!(rig.device.lock().writes.pop(), Some((8, 0xaabb_ccdd)));
    }

    #[test]
    fn verify_nonzero_slot_config_accesses_do_not_touch_device() {
        let rig = VpciTestRig::new();
        let writes_before = rig.device.lock().writes.len();

        rig.bus
            .lock()
            .mmio_write(VpciTestRig::slot_addr(), &1u32.to_ne_bytes())
            .unwrap();

        let mut read_data = [0; 4];
        rig.bus
            .lock()
            .mmio_read(VpciTestRig::config_addr(0), &mut read_data)
            .unwrap();
        assert_eq!(read_data, [0xff; 4]);

        rig.bus
            .lock()
            .mmio_write(VpciTestRig::config_addr(0), &0xaabb_ccddu32.to_ne_bytes())
            .unwrap();
        assert!(rig.device.lock().pending_read.is_none());
        assert!(rig.device.lock().pending_write.is_none());
        assert_eq!(rig.device.lock().writes.len(), writes_before);
    }
}
