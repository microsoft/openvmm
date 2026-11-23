// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exports [`ArcMutexChipsetServices`].

use self::device_range::DeviceRangeMapper;
use super::device::ArcMutexChipsetServicesFinalize;
use super::state_unit::ArcMutexChipsetDeviceUnit;
use super::state_unit::DynamicDeviceUnit;
use crate::BusIdPci;
use crate::BusIdPcieDownstreamPort;
use crate::BusIdPcieEnumerator;
use crate::ChipsetBuilder;
use crate::VmmChipsetDevice;
use crate::chipset::io_ranges::IoRanges;
use crate::chipset::line_sets::LineSetTargetDevice;
use chipset_device::ChipsetDevice;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device_resources::LineSetId;
use closeable_mutex::CloseableMutex;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::Weak;
use thiserror::Error;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::line_interrupt::NewLineError;
use vmcore::vmtime::VmTimeSource;

/// The concrete instance of [`ChipsetServices`] offered to devices when using
/// the `Weak<CloseableMutex<..>>` [`Chipset`](crate::Chipset) API.
pub struct ArcMutexChipsetServices<'a, 'b> {
    builder: &'a ChipsetBuilder<'b>,
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    dev_name: Arc<str>,
    line_set_dependencies: Vec<LineSetId>,
    line_set_targets: Vec<(LineSetId, RangeInclusive<u32>, u32)>,
    line_error: Option<NewLineError>,
    static_mmio: Vec<Box<dyn ControlMmioIntercept>>,
    static_pio: Vec<Box<dyn ControlPortIoIntercept>>,
    // The device's places on the PCI and PCIe buses, which `finalize`
    // registers once the add can no longer fail.
    static_pci: Vec<(BusIdPci, (u8, u8, u8))>,
    static_pcie: Vec<BusIdPcieDownstreamPort>,
    static_pcie_rcieps: Vec<(BusIdPcieEnumerator, u8)>,

    omit_saved_state: bool,
}

#[derive(Debug, Error)]
pub enum FinalizeError {
    #[error("failed to create line interrupt")]
    NewLine(#[source] NewLineError),
    #[error("missing line interrupt target support")]
    NoInterruptTarget,
    #[error("eoi handler already set")]
    EoiHandlerAlreadySet,
    #[error("pic handler already set")]
    PicHandlerAlreadySet,
    #[error("name already in use")]
    NameInUse(#[source] state_unit::NameInUse),
}

impl<T: VmmChipsetDevice> ArcMutexChipsetServicesFinalize<T> for ArcMutexChipsetServices<'_, '_> {
    type Error = FinalizeError;

    fn finalize(
        self,
        device: &Arc<CloseableMutex<T>>,
        dev_name: Arc<str>,
    ) -> Result<(), Self::Error> {
        if let Some(err) = self.line_error {
            return Err(FinalizeError::NewLine(err));
        }

        let handle_eoi;
        let acknowledge_pic_interrupt;
        let line_target;

        let mut inner = self.builder.inner.lock();

        let mut builder = {
            let mut device = device.lock();
            handle_eoi = device.supports_handle_eoi().is_some();
            acknowledge_pic_interrupt = device.supports_acknowledge_pic_interrupt().is_some();
            line_target = device
                .supports_line_interrupt_target()
                .map(|d| d.valid_lines().to_vec());

            let mut builder = self.builder.units.add(dev_name.clone());
            if device.advances_time() {
                builder = builder.advances_time();
            }
            // Before stopping the device, the chipset interface must stop
            // running so that it stops issuing MMIO/PIO requests.
            if device.supports_mmio().is_some()
                || device.supports_pio().is_some()
                || device.supports_pci().is_some()
                || device.supports_handle_eoi().is_some()
                || device.supports_acknowledge_pic_interrupt().is_some()
            {
                builder = builder.dependency_of(&self.builder.chipset_unit);
            }
            // Make all devices depend on vmtime to avoid having to track this
            // precisely.
            builder.depends_on(self.builder.vmtime_unit)
        };

        // Check what can fail first, ending with the spawn of the device's
        // unit, and only then make the device the chipset's EOI handler, PIC,
        // and line target and place it on its PCI or PCIe bus, so that a
        // failed add leaves none of them behind.
        if handle_eoi && inner.vm_chipset.eoi_handler.is_some() {
            return Err(FinalizeError::EoiHandlerAlreadySet);
        }
        if acknowledge_pic_interrupt && inner.vm_chipset.pic.is_some() {
            return Err(FinalizeError::PicHandlerAlreadySet);
        }

        for id in self.line_set_dependencies {
            let (_, handle) =
                inner
                    .line_sets
                    .line_set(self.builder.driver_source, self.builder.units, id);
            builder = builder.depends_on(handle);
        }

        if !self.line_set_targets.is_empty() {
            let valid_lines = line_target.ok_or(FinalizeError::NoInterruptTarget)?;
            for (id, source_range, target_start) in &self.line_set_targets {
                assert!(valid_lines.iter().any(|range| {
                    *range.start() <= *target_start
                        && *range.end()
                            >= *target_start + (source_range.end() - source_range.start())
                }));
                let (_, handle) = inner.line_sets.line_set(
                    self.builder.driver_source,
                    self.builder.units,
                    id.clone(),
                );
                builder = builder.dependency_of(handle);
            }
        }

        let device_unit = ArcMutexChipsetDeviceUnit::new(device.clone(), self.omit_saved_state)
            .with_static_regions(self.static_mmio, self.static_pio);
        let driver = self.builder.driver_source.simple();
        let unit = builder
            .spawn(driver.clone(), |recv| device_unit.run(recv))
            .map_err(FinalizeError::NameInUse)?;

        if handle_eoi {
            inner.vm_chipset.eoi_handler = Some(device.clone());
        }
        if acknowledge_pic_interrupt {
            inner.vm_chipset.pic = Some(device.clone());
        }

        if !self.line_set_targets.is_empty() {
            let device = Arc::new(LineSetTargetDevice::new(device.clone()));
            for (id, source_range, target_start) in self.line_set_targets {
                let (line_set, _) =
                    inner
                        .line_sets
                        .line_set(self.builder.driver_source, self.builder.units, id);
                line_set.add_target(source_range, target_start, dev_name.clone(), device.clone());
            }
        }

        for (bus_id, bdf) in self.static_pci {
            inner.bus_resolver.register_weak_mutex_pci_device(
                bus_id,
                bdf,
                dev_name.clone(),
                self.dev.clone(),
            );
        }
        for bus_id_port in self.static_pcie {
            inner.bus_resolver.register_weak_mutex_pcie_device(
                bus_id_port,
                dev_name.clone(),
                self.dev.clone(),
            );
        }
        for (enumerator_id, devfn) in self.static_pcie_rcieps {
            inner.bus_resolver.register_weak_mutex_pcie_rciep(
                enumerator_id,
                devfn,
                dev_name.clone(),
                self.dev.clone(),
            );
        }

        inner
            .arc_mutex_device_units
            .push(DynamicDeviceUnit::new(unit, driver, device.clone()));
        Ok(())
    }
}

impl<'a, 'b> ArcMutexChipsetServices<'a, 'b> {
    pub fn new(
        builder: &'a ChipsetBuilder<'b>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        dev_name: Arc<str>,
    ) -> Self {
        Self {
            builder,
            dev,
            dev_name,
            line_set_dependencies: Vec::new(),
            line_set_targets: Vec::new(),
            line_error: None,
            static_mmio: Vec::new(),
            static_pio: Vec::new(),
            static_pci: Vec::new(),
            static_pcie: Vec::new(),
            static_pcie_rcieps: Vec::new(),

            omit_saved_state: false,
        }
    }

    pub fn omit_saved_state(&mut self) {
        self.omit_saved_state = true
    }

    pub fn register_vmtime(&self) -> &VmTimeSource {
        self.builder.vmtime
    }

    pub fn register_mmio(&self) -> DeviceRangeMapper<u64> {
        let inner = self.builder.inner.lock();
        DeviceRangeMapper {
            dev: self.dev.clone(),
            dev_name: self.dev_name.clone(),
            ranges: inner.vm_chipset.mmio_ranges.clone(),
        }
    }

    pub fn register_pio(&self) -> DeviceRangeMapper<u16> {
        let inner = self.builder.inner.lock();
        DeviceRangeMapper {
            dev: self.dev.clone(),
            dev_name: self.dev_name.clone(),
            ranges: inner.vm_chipset.pio_ranges.clone(),
        }
    }

    pub fn register_static_mmio(&mut self, label: &str, range: RangeInclusive<u64>) {
        let mut region = self
            .register_mmio()
            .new_io_region(label, range.end() - range.start() + 1);
        region.map(*range.start());
        self.static_mmio.push(region);
    }

    pub fn register_static_pio(&mut self, label: &str, range: RangeInclusive<u16>) {
        let mut region = self
            .register_pio()
            .new_io_region(label, range.end() - range.start() + 1);
        region.map(*range.start());
        self.static_pio.push(region);
    }

    pub fn register_static_pci(&mut self, bus_id: BusIdPci, bdf: (u8, u8, u8)) {
        self.static_pci.push((bus_id, bdf));
    }

    pub fn register_static_pcie(&mut self, bus_id: BusIdPcieDownstreamPort) {
        self.static_pcie.push(bus_id);
    }

    pub fn register_static_pcie_rciep(&mut self, enumerator_id: BusIdPcieEnumerator, devfn: u8) {
        self.static_pcie_rcieps.push((enumerator_id, devfn));
    }

    pub fn new_line(&mut self, id: LineSetId, name: &str, vector: u32) -> LineInterrupt {
        let mut inner = self.builder.inner.lock();
        let (line_set, _) =
            inner
                .line_sets
                .line_set(self.builder.driver_source, self.builder.units, id.clone());
        match line_set.new_line(vector, format!("{}:{}", self.dev_name, name)) {
            Ok(line) => {
                self.line_set_dependencies.push(id);
                line
            }
            Err(err) => {
                // Simplify the caller's error handling by returning a detached
                // line and storing the error to propagate later.
                self.line_error.get_or_insert(err);
                LineInterrupt::detached()
            }
        }
    }

    pub fn add_line_target(
        &mut self,
        id: LineSetId,
        source_range: RangeInclusive<u32>,
        target_start: u32,
    ) {
        self.line_set_targets.push((id, source_range, target_start));
    }
}

pub(crate) fn register_mmio_for_device(
    dev_name: Arc<str>,
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    ranges: IoRanges<u64>,
) -> impl RegisterMmioIntercept {
    DeviceRangeMapper {
        dev,
        dev_name,
        ranges,
    }
}

mod device_range {
    use crate::chipset::io_ranges::IoRanges;
    use address_filter::RangeKey;
    use chipset_device::ChipsetDevice;
    use chipset_device::mmio::ControlMmioIntercept;
    use chipset_device::mmio::RegisterMmioIntercept;
    use chipset_device::pio::ControlPortIoIntercept;
    use chipset_device::pio::RegisterPortIoIntercept;
    use closeable_mutex::CloseableMutex;
    use std::sync::Arc;
    use std::sync::Weak;

    /// A concrete type which implements [`RegisterMmioIntercept`] or
    /// [`RegisterPortIoIntercept`] (depending on whether T is u64 or u16)
    pub struct DeviceRangeMapper<T> {
        pub(super) dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        pub(super) dev_name: Arc<str>,
        pub(super) ranges: IoRanges<T>,
    }

    // Implementation detail - the concrete type returned by DeviceRangeMapper's
    // `new_io_region` implementation
    struct DeviceRange<T: RangeKey> {
        ranges: IoRanges<T>,
        region_name: Arc<str>,
        len: T,
        addr: Option<T>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        dev_name: Arc<str>,
    }

    impl<T: RangeKey> DeviceRange<T> {
        fn revoke(&mut self) {
            if let Some(addr) = self.addr.take() {
                self.ranges.revoke(addr);
            }
        }
    }

    impl<T: RangeKey> Drop for DeviceRange<T> {
        fn drop(&mut self) {
            self.revoke();
        }
    }

    macro_rules! impl_device_range {
        ($register:ident, $control:ident, $addr:ty) => {
            impl $register for DeviceRangeMapper<$addr> {
                fn new_io_region(&mut self, region_name: &str, len: $addr) -> Box<dyn $control> {
                    Box::new(DeviceRange {
                        ranges: self.ranges.clone(),
                        region_name: region_name.into(),
                        len,
                        addr: None,
                        dev: self.dev.clone(),
                        dev_name: self.dev_name.clone(),
                    })
                }
            }

            impl $control for DeviceRange<$addr> {
                fn region_name(&self) -> &str {
                    &self.region_name
                }

                fn map(&mut self, addr: $addr) {
                    tracing::trace!(region_name = ?self.region_name, ?addr, len = ?self.len, "map");
                    if self.len == 0 {
                        return;
                    }
                    self.unmap();
                    match self.ranges.register(
                        addr,
                        addr.checked_add(self.len - 1).expect("overflow during addition, not possible in real hardware"),
                        self.region_name.clone(),
                        self.dev.clone(),
                        self.dev_name.clone(),
                    ) {
                        Ok(()) => {
                            self.addr = Some(addr);
                        }
                        Err(conflict) => {
                            // TODO?: switch behavior such that incoming mappings
                            // will unmap any previously mapped region they overlap
                            tracing::warn!(
                                conflict = %conflict,
                                "{}::map failed",
                                stringify!($control)
                            );
                        }
                    }
                }

                fn unmap(&mut self) {
                    tracing::trace!(region_name = ?self.region_name, addr = ?self.addr, len = ?self.len, "unmap");
                    self.revoke();
                }

                fn addr(&self) -> Option<$addr> {
                    self.addr
                }

                fn len(&self) -> $addr {
                    self.len
                }

                fn offset_of(&self, addr: $addr) -> Option<$addr> {
                    let base = self.addr?;
                    // Compare the offset with the length: the exclusive end of
                    // a region that ends at the top of the address space does
                    // not fit in the address type.
                    let offset = addr.checked_sub(base)?;
                    (offset < self.len).then_some(offset)
                }
            }
        };
    }

    impl_device_range!(RegisterMmioIntercept, ControlMmioIntercept, u64);
    impl_device_range!(RegisterPortIoIntercept, ControlPortIoIntercept, u16);

    #[cfg(test)]
    mod tests {
        use super::*;
        use missing_dev::MissingDev;
        use test_with_tracing::test;

        fn mapper<T>(ranges: IoRanges<T>) -> DeviceRangeMapper<T> {
            DeviceRangeMapper {
                dev: Weak::<CloseableMutex<MissingDev>>::new(),
                dev_name: "test".into(),
                ranges,
            }
        }

        #[test]
        fn mmio_region_drop_releases_only_its_current_mapping() {
            let ranges = IoRanges::<u64>::new(false, None);
            let mut mapper = mapper(ranges.clone());
            let mut region = mapper.new_io_region("mmio", 4);
            region.map(0x1000);
            assert!(ranges.is_occupied(0x1000));
            region.map(0x2000);
            assert!(!ranges.is_occupied(0x1000));
            assert!(ranges.is_occupied(0x2000));

            let mut conflicting = mapper.new_io_region("conflicting", 4);
            conflicting.map(0x2000);
            drop(conflicting);
            assert!(ranges.is_occupied(0x2000));

            region.unmap();
            let mut replacement = mapper.new_io_region("replacement", 4);
            replacement.map(0x2000);
            drop(region);
            assert!(ranges.is_occupied(0x2000));
            drop(replacement);
            assert!(!ranges.is_occupied(0x2000));
        }

        #[test]
        fn pio_region_drop_releases_its_mapping() {
            let ranges = IoRanges::<u16>::new(false, None);
            let mut mapper = mapper(ranges.clone());
            let mut region = mapper.new_io_region("pio", 4);
            region.map(0x100);
            assert!(ranges.is_occupied(0x100));
            drop(region);
            assert!(!ranges.is_occupied(0x100));
        }

        #[test]
        fn pio_region_ending_at_the_top_finds_its_offsets() {
            let mut mapper = mapper(IoRanges::<u16>::new(false, None));
            let mut region = mapper.new_io_region("top", 2);
            assert_eq!(region.offset_of(0xffff), None);
            region.map(0xfffe);
            assert_eq!(region.addr(), Some(0xfffe));
            assert_eq!(region.offset_of(0xfffd), None);
            assert_eq!(region.offset_of(0xfffe), Some(0));
            assert_eq!(region.offset_of(0xffff), Some(1));
            region.map(0xfffd);
            assert_eq!(region.offset_of(0xfffd), Some(0));
            assert_eq!(region.offset_of(0xfffe), Some(1));
            assert_eq!(region.offset_of(0xffff), None);
            region.unmap();
            assert_eq!(region.offset_of(0xfffe), None);
        }

        #[test]
        fn mmio_region_ending_at_the_top_finds_its_offsets() {
            let mut mapper = mapper(IoRanges::<u64>::new(false, None));
            let mut region = mapper.new_io_region("top", 0x1000);
            let base = u64::MAX - 0xfff;
            assert_eq!(region.offset_of(u64::MAX), None);
            region.map(base);
            assert_eq!(region.addr(), Some(base));
            assert_eq!(region.offset_of(base - 1), None);
            assert_eq!(region.offset_of(base), Some(0));
            assert_eq!(region.offset_of(u64::MAX), Some(0xfff));
            region.map(base - 1);
            assert_eq!(region.offset_of(base - 1), Some(0));
            assert_eq!(region.offset_of(u64::MAX - 1), Some(0xfff));
            assert_eq!(region.offset_of(u64::MAX), None);
            region.unmap();
            assert_eq!(region.offset_of(base), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FinalizeError;
    use crate::BusIdPci;
    use crate::BusIdPcieDownstreamPort;
    use crate::BusIdPcieEnumerator;
    use crate::Chipset;
    use crate::ChipsetBuilder;
    use crate::DebugEventHandler;
    use crate::chipset::PcieConflict;
    use crate::chipset::PcieConflictReason;
    use crate::chipset::backing::arc_mutex::device::AddDeviceError;
    use crate::chipset::backing::arc_mutex::pci::RegisterWeakMutexPcie;
    use chipset_device::ChipsetDevice;
    use chipset_device::interrupt::AcknowledgePicInterrupt;
    use chipset_device::interrupt::HandleEoi;
    use chipset_device::interrupt::LineInterruptTarget;
    use chipset_device::io::IoResult;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigSpace;
    use chipset_device_resources::IRQ_LINE_SET;
    use closeable_mutex::CloseableMutex;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use pci_bus::GenericPciBus;
    use pcie::root::DownstreamPortInfo;
    use state_unit::StateRequest;
    use state_unit::StateUnits;
    use state_unit::UnitHandle;
    use std::ops::RangeInclusive;
    use std::sync::Arc;
    use std::sync::Weak;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;
    use vmcore::vmtime::VmTimeSource;

    /// The vector that a [`Recorder`] acknowledges as the PIC.
    const PIC_VECTOR: u8 = 0x30;

    /// A call that the chipset made to a [`Recorder`].
    #[derive(Debug, PartialEq)]
    enum Call {
        Irq(u32, bool),
        Eoi(u32),
        AcknowledgePic,
    }

    /// A device that records each call that the chipset makes to it: as a
    /// target of the line sets and, if it is the interrupt controller, as the
    /// EOI handler and the PIC.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct Recorder {
        calls: Arc<Mutex<Vec<Call>>>,
        interrupt_controller: bool,
    }

    impl ChipsetDevice for Recorder {
        fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
            Some(self)
        }

        fn supports_handle_eoi(&mut self) -> Option<&mut dyn HandleEoi> {
            if self.interrupt_controller {
                Some(self)
            } else {
                None
            }
        }

        fn supports_acknowledge_pic_interrupt(
            &mut self,
        ) -> Option<&mut dyn AcknowledgePicInterrupt> {
            if self.interrupt_controller {
                Some(self)
            } else {
                None
            }
        }
    }

    impl LineInterruptTarget for Recorder {
        fn set_irq(&mut self, vector: u32, high: bool) {
            self.calls.lock().push(Call::Irq(vector, high));
        }

        fn valid_lines(&self) -> &[RangeInclusive<u32>] {
            &[0..=1]
        }
    }

    impl HandleEoi for Recorder {
        fn handle_eoi(&mut self, vector: u32) {
            self.calls.lock().push(Call::Eoi(vector));
        }
    }

    impl AcknowledgePicInterrupt for Recorder {
        fn acknowledge_interrupt(&mut self) -> Option<u8> {
            self.calls.lock().push(Call::AcknowledgePic);
            Some(PIC_VECTOR)
        }
    }

    impl ChangeDeviceState for Recorder {
        fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl SaveRestore for Recorder {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    struct NoDebugEvents;

    impl DebugEventHandler for NoDebugEvents {
        fn on_debug_break(&self, _: Option<u32>) {
            panic!("unexpected debug break");
        }
    }

    /// What a chipset builder borrows: a driver source, VM time, and the state
    /// units, with VM time's.
    struct Foundation {
        driver_source: VmTaskDriverSource,
        units: StateUnits,
        vmtime: VmTimeSource,
        vmtime_unit: UnitHandle,
        _vmtime_keeper: VmTimeKeeper,
        _vmtime_requests: mesh::Receiver<StateRequest>,
    }

    impl Foundation {
        async fn new(driver: &DefaultDriver) -> Self {
            let units = StateUnits::new();
            let (send, vmtime_requests) = mesh::channel();
            let vmtime_unit = units.add("vmtime").build(send).unwrap();
            let vmtime_keeper = VmTimeKeeper::new(driver, VmTime::from_100ns(0));
            let vmtime = vmtime_keeper.builder().build(driver).await.unwrap();
            Self {
                driver_source: VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone())),
                units,
                vmtime,
                vmtime_unit,
                _vmtime_keeper: vmtime_keeper,
                _vmtime_requests: vmtime_requests,
            }
        }

        fn builder(&self) -> ChipsetBuilder<'_> {
            ChipsetBuilder::new(
                &self.driver_source,
                &self.units,
                Arc::new(NoDebugEvents),
                &self.vmtime,
                &self.vmtime_unit,
                false,
                false,
                None,
            )
        }

        /// Gives `name` to another unit, so that a device added under it
        /// fails as its unit spawns, the last step of its add.
        fn take_name(&self, name: &str) -> UnitHandle {
            self.units.add(name).build(mesh::channel().0).unwrap()
        }
    }

    /// A new line of the builder's IRQ line set, as a device's `new_line`
    /// makes one.
    fn irq_line(builder: &ChipsetBuilder<'_>, vector: u32) -> LineInterrupt {
        let mut inner = builder.inner.lock();
        let (line_set, _) =
            inner
                .line_sets
                .line_set(builder.driver_source, builder.units, IRQ_LINE_SET);
        line_set.new_line(vector, "source").unwrap()
    }

    /// Whether a device's add failed as its unit spawned, because the unit's
    /// name was in use.
    fn is_name_in_use(err: &AddDeviceError) -> bool {
        use std::error::Error;
        err.source()
            .and_then(|kind| kind.source())
            .and_then(|err| err.downcast_ref::<FinalizeError>())
            .is_some_and(|err| matches!(err, FinalizeError::NameInUse(_)))
    }

    /// A device whose add fails as its unit spawns targets none of the line
    /// sets that it asked to: neither a line that is high as the add fails nor
    /// one made after it reaches the device, while both reach the device
    /// added in its place.
    #[async_test]
    async fn failed_add_leaves_no_line_target(driver: DefaultDriver) {
        let foundation = Foundation::new(&driver).await;
        let builder = foundation.builder();
        let _taken = foundation.take_name("device");
        let early = irq_line(&builder, 0);
        early.set_level(true);

        let failed = Arc::new(Mutex::new(Vec::new()));
        let Err(err) = builder.arc_mutex_device("device").add(|services| {
            services.add_line_target(IRQ_LINE_SET, 0..=1, 0);
            Recorder {
                calls: failed.clone(),
                interrupt_controller: false,
            }
        }) else {
            panic!("a device was added under another unit's name");
        };
        assert!(is_name_in_use(&err), "{err:?}");

        let added = Arc::new(Mutex::new(Vec::new()));
        builder
            .arc_mutex_device("replacement")
            .add(|services| {
                services.add_line_target(IRQ_LINE_SET, 0..=1, 0);
                Recorder {
                    calls: added.clone(),
                    interrupt_controller: false,
                }
            })
            .unwrap();
        let late = irq_line(&builder, 1);
        early.set_level(false);
        late.set_level(true);

        assert_eq!(*failed.lock(), []);
        assert_eq!(
            *added.lock(),
            [Call::Irq(0, true), Call::Irq(0, false), Call::Irq(1, true)]
        );
    }

    /// A device whose add fails as its unit spawns leaves the chipset's EOI
    /// handler and PIC to the device added in its place, which receives the
    /// EOIs and the PIC's acknowledgments.
    #[async_test]
    async fn failed_add_leaves_the_interrupt_controller_free(driver: DefaultDriver) {
        let foundation = Foundation::new(&driver).await;
        let builder = foundation.builder();
        let _taken = foundation.take_name("device");

        let failed = Arc::new(Mutex::new(Vec::new()));
        let Err(err) = builder.arc_mutex_device("device").add(|_| Recorder {
            calls: failed.clone(),
            interrupt_controller: true,
        }) else {
            panic!("a device was added under another unit's name");
        };
        assert!(is_name_in_use(&err), "{err:?}");

        let added = Arc::new(Mutex::new(Vec::new()));
        builder
            .arc_mutex_device("replacement")
            .add(|_| Recorder {
                calls: added.clone(),
                interrupt_controller: true,
            })
            .unwrap();
        let (chipset, _devices) = builder.build().unwrap();
        chipset.handle_eoi(5);
        assert_eq!(chipset.acknowledge_pic_interrupt(), Some(PIC_VECTOR));

        assert_eq!(*failed.lock(), []);
        assert_eq!(*added.lock(), [Call::Eoi(5), Call::AcknowledgePic]);
    }

    /// The configuration address and data ports of the tests' PCI bus.
    const PCI_ADDRESS_PORT: u16 = 0xcf8;
    const PCI_DATA_PORT: u16 = 0xcfc;

    /// The devfns of the two downstream ports of a [`RootComplex`], and the
    /// devfns at which the tests add RCiEPs to it.
    const PORT_DEVFN: u8 = 0x08;
    const NEIGHBOR_PORT_DEVFN: u8 = 0x10;
    const RCIEP_DEVFN: u8 = 0x18;
    const NEIGHBOR_RCIEP_DEVFN: u8 = 0x20;

    /// A PCI function whose vendor and device IDs read as its value.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct PciFunction(u32);

    impl ChipsetDevice for PciFunction {
        fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
            Some(self)
        }
    }

    impl PciConfigSpace for PciFunction {
        fn pci_cfg_read(&mut self, offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
            value.set(if offset == 0 { self.0 } else { 0 });
            IoResult::Ok
        }

        fn pci_cfg_write(&mut self, _: u16, _: ByteEnabledDwordWrite) -> IoResult {
            IoResult::Ok
        }
    }

    impl ChangeDeviceState for PciFunction {
        fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl SaveRestore for PciFunction {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, _: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// The devices that a [`RootComplex`] attached, in order, with their
    /// devfns and names.
    type Attached = Arc<Mutex<Vec<(u8, Arc<str>, Weak<CloseableMutex<dyn ChipsetDevice>>)>>>;

    /// A PCIe root complex with two downstream ports, which takes RCiEPs too
    /// and, as a real one does, one device at each devfn.
    struct RootComplex(Attached);

    impl RootComplex {
        fn attach(
            &mut self,
            devfn: u8,
            name: Arc<str>,
            device: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            let mut attached = self.0.lock();
            if let Some((_, existing, _)) = attached.iter().find(|(d, _, _)| *d == devfn) {
                return Err(PcieConflict {
                    conflict_dev: name,
                    reason: PcieConflictReason::ExistingDev(existing.clone()),
                });
            }
            attached.push((devfn, name, device));
            Ok(())
        }
    }

    impl RegisterWeakMutexPcie for RootComplex {
        fn add_pcie_device(
            &mut self,
            port_devfn: u8,
            name: Arc<str>,
            device: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.attach(port_devfn, name, device)
        }

        fn downstream_ports(&self) -> Vec<DownstreamPortInfo> {
            [("port", PORT_DEVFN), ("neighbor port", NEIGHBOR_PORT_DEVFN)]
                .map(|(name, devfn)| DownstreamPortInfo {
                    devfn,
                    name: name.into(),
                    bus_range: Default::default(),
                })
                .into()
        }

        fn add_rciep(
            &mut self,
            devfn: u8,
            name: Arc<str>,
            device: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.attach(devfn, name, device)
        }
    }

    /// Reads the vendor and device IDs of the function at `device` and
    /// `function` on PCI bus 0, through the bus's configuration ports.
    async fn read_pci_id(chipset: &Chipset, device: u8, function: u8) -> u32 {
        let address = (1 << 31) | (u32::from(device) << 11) | (u32::from(function) << 8);
        chipset
            .io_write(0, PCI_ADDRESS_PORT, &address.to_le_bytes())
            .await;
        let mut id = [0; 4];
        chipset.io_read(0, PCI_DATA_PORT, &mut id).await;
        u32::from_le_bytes(id)
    }

    /// A PCI function whose add fails as its unit spawns leaves its bus,
    /// device, and function free: the function added there in its place joins
    /// the bus and answers its configuration reads, as the function added
    /// beside it before the failure still does.
    #[async_test]
    async fn failed_add_leaves_the_pci_slot_free(driver: DefaultDriver) {
        let foundation = Foundation::new(&driver).await;
        let builder = foundation.builder();
        let bus = builder
            .arc_mutex_device("pci_bus")
            .add(|services| {
                GenericPciBus::new(
                    &mut services.register_pio(),
                    PCI_ADDRESS_PORT,
                    PCI_DATA_PORT,
                )
            })
            .unwrap();
        builder.register_weak_mutex_pci_bus(BusIdPci::new("pci"), Box::new(bus));
        let add_function = |name: &str, function, id| {
            builder
                .arc_mutex_device(name)
                .on_pci_bus(BusIdPci::new("pci"))
                .with_pci_addr(0, 5, function)
                .add(|_| PciFunction(id))
        };

        add_function("neighbor", 1, 0x1111_1414).unwrap();
        let _taken = foundation.take_name("device");
        let Err(err) = add_function("device", 0, 0x2222_1414) else {
            panic!("a device was added under another unit's name");
        };
        assert!(is_name_in_use(&err), "{err:?}");
        add_function("replacement", 0, 0x3333_1414).unwrap();

        let (chipset, _devices) = builder.build().unwrap();
        assert_eq!(read_pci_id(&chipset, 5, 0).await, 0x3333_1414);
        assert_eq!(read_pci_id(&chipset, 5, 1).await, 0x1111_1414);
    }

    /// A PCIe function whose add fails as its unit spawns leaves its
    /// downstream port, or its devfn as an RCiEP, to the function added in its
    /// place, while the functions added beside it before the failure stay
    /// attached.
    #[async_test]
    async fn failed_add_leaves_the_pcie_port_and_rciep_free(driver: DefaultDriver) {
        let foundation = Foundation::new(&driver).await;
        let builder = foundation.builder();
        let attached = Attached::default();
        builder.register_weak_mutex_pcie_enumerator(
            BusIdPcieEnumerator::new("root"),
            Box::new(RootComplex(attached.clone())),
        );
        let on_port = |port: &str, name: &str| {
            builder
                .arc_mutex_device(name)
                .on_pcie_port(BusIdPcieDownstreamPort::new(port))
                .add(|_| PciFunction(0))
        };
        let as_rciep = |devfn, name: &str| {
            builder
                .arc_mutex_device(name)
                .on_pcie_root_complex(BusIdPcieEnumerator::new("root"), devfn)
                .add(|_| PciFunction(0))
        };

        on_port("neighbor port", "endpoint neighbor").unwrap();
        as_rciep(NEIGHBOR_RCIEP_DEVFN, "rciep neighbor").unwrap();
        let _taken = [
            foundation.take_name("endpoint"),
            foundation.take_name("rciep"),
        ];
        for result in [on_port("port", "endpoint"), as_rciep(RCIEP_DEVFN, "rciep")] {
            let Err(err) = result else {
                panic!("a device was added under another unit's name");
            };
            assert!(is_name_in_use(&err), "{err:?}");
        }
        on_port("port", "endpoint replacement").unwrap();
        as_rciep(RCIEP_DEVFN, "rciep replacement").unwrap();

        let (_chipset, _devices) = builder.build().unwrap();
        let mut attached = attached
            .lock()
            .iter()
            .map(|(devfn, name, device)| (*devfn, name.to_string(), device.upgrade().is_some()))
            .collect::<Vec<_>>();
        attached.sort();
        assert_eq!(
            attached,
            [
                (PORT_DEVFN, "endpoint replacement".to_owned(), true),
                (NEIGHBOR_PORT_DEVFN, "endpoint neighbor".to_owned(), true),
                (RCIEP_DEVFN, "rciep replacement".to_owned(), true),
                (NEIGHBOR_RCIEP_DEVFN, "rciep neighbor".to_owned(), true),
            ]
        );
    }
}
