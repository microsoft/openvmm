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
use crate::chipset::contract::pci_identity;
use crate::chipset::contract::region_descriptor;
use crate::chipset::io_ranges::IoRanges;
use crate::chipset::line_sets::LineSetTargetDevice;
use chipset_device::ChipsetDevice;
use chipset_device::mmio::ControlMmioIntercept;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device::pio::ControlPortIoIntercept;
use chipset_device::pio::RegisterPortIoIntercept;
use chipset_device_resources::LineSetId;
use closeable_mutex::CloseableMutex;
use state_unit::UnitDescriptor;
use state_unit::UnitLifetime;
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

    omit_saved_state: bool,

    /// The guest-visible properties that the device's registrations declare,
    /// which its state unit's contract records.
    descriptors: Vec<UnitDescriptor>,
    stable_id: Option<String>,
    kind: String,
    lifetime: UnitLifetime,
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

            let mut builder = self
                .builder
                .units
                .add(dev_name.clone())
                .kind(self.kind)
                .lifetime(self.lifetime)
                .descriptors(self.descriptors);
            if let Some(stable_id) = self.stable_id {
                builder = builder.stable_id(stable_id);
            }
            if let Some(pci) = device.supports_pci() {
                builder = builder.descriptors(pci_identity(pci));
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

        if handle_eoi {
            if inner.vm_chipset.eoi_handler.is_some() {
                return Err(FinalizeError::EoiHandlerAlreadySet);
            }
            inner.vm_chipset.eoi_handler = Some(device.clone());
        }
        if acknowledge_pic_interrupt {
            if inner.vm_chipset.pic.is_some() {
                return Err(FinalizeError::PicHandlerAlreadySet);
            }
            inner.vm_chipset.pic = Some(device.clone());
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
            let device = Arc::new(LineSetTargetDevice::new(device.clone()));
            for (id, source_range, target_start) in self.line_set_targets {
                assert!(valid_lines.iter().any(|range| {
                    *range.start() <= target_start
                        && *range.end()
                            >= target_start + (source_range.end() - source_range.start())
                }));
                let (line_set, handle) =
                    inner
                        .line_sets
                        .line_set(self.builder.driver_source, self.builder.units, id);
                line_set.add_target(source_range, target_start, dev_name.clone(), device.clone());
                builder = builder.dependency_of(handle);
            }
        }

        let device_unit = ArcMutexChipsetDeviceUnit::new(device.clone(), self.omit_saved_state)
            .with_static_regions(self.static_mmio, self.static_pio);
        let driver = self.builder.driver_source.simple();
        let unit = builder
            .spawn(driver.clone(), |recv| device_unit.run(recv))
            .map_err(FinalizeError::NameInUse)?;

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

            omit_saved_state: false,

            descriptors: Vec::new(),
            stable_id: None,
            kind: String::new(),
            lifetime: UnitLifetime::Static,
        }
    }

    pub fn omit_saved_state(&mut self) {
        self.omit_saved_state = true
    }

    /// Sets the guest-visible identity that the device's state unit records,
    /// such as the PCIe port that it occupies. Defaults to the device's name.
    pub fn set_stable_id(&mut self, stable_id: String) {
        self.stable_id = Some(stable_id);
    }

    /// Sets what the device's state unit records the device to be, such as
    /// its resource kind.
    pub fn set_kind(&mut self, kind: String) {
        self.kind = kind;
    }

    /// Sets how the device became part of the VM, which its state unit
    /// records. Defaults to [`UnitLifetime::Static`].
    pub fn set_lifetime(&mut self, lifetime: UnitLifetime) {
        self.lifetime = lifetime;
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
        self.descriptors
            .push(region_descriptor("mmio", label, &range));
        let mut region = self
            .register_mmio()
            .new_io_region(label, range.end() - range.start() + 1);
        region.map(*range.start());
        self.static_mmio.push(region);
    }

    pub fn register_static_pio(&mut self, label: &str, range: RangeInclusive<u16>) {
        self.descriptors
            .push(region_descriptor("pio", label, &range));
        let mut region = self
            .register_pio()
            .new_io_region(label, range.end() - range.start() + 1);
        region.map(*range.start());
        self.static_pio.push(region);
    }

    pub fn register_static_pci(&mut self, bus_id: BusIdPci, bdf: (u8, u8, u8)) {
        let (bus, device, function) = bdf;
        self.descriptors.push(UnitDescriptor::new(
            "pci.bdf",
            format!("{}/{bus:02x}:{device:02x}.{function}", bus_id.name),
        ));
        self.builder.register_weak_mutex_pci_device(
            bus_id,
            bdf,
            self.dev_name.clone(),
            self.dev.clone(),
        );
    }

    pub fn register_static_pcie(&mut self, bus_id: BusIdPcieDownstreamPort) {
        self.descriptors
            .push(UnitDescriptor::new("pcie.port", bus_id.name.as_ref()));
        self.builder.register_weak_mutex_pcie_device(
            bus_id,
            self.dev_name.clone(),
            self.dev.clone(),
        );
    }

    pub fn register_static_pcie_rciep(&mut self, enumerator_id: BusIdPcieEnumerator, devfn: u8) {
        self.descriptors.push(UnitDescriptor::new(
            "pcie.rciep",
            format!("{}/{devfn:#x}", enumerator_id.name),
        ));
        self.builder.register_weak_mutex_pcie_rciep(
            enumerator_id,
            devfn,
            self.dev_name.clone(),
            self.dev.clone(),
        );
    }

    pub fn new_line(&mut self, id: LineSetId, name: &str, vector: u32) -> LineInterrupt {
        let mut inner = self.builder.inner.lock();
        let (line_set, _) =
            inner
                .line_sets
                .line_set(self.builder.driver_source, self.builder.units, id.clone());
        match line_set.new_line(vector, format!("{}:{}", self.dev_name, name)) {
            Ok(line) => {
                self.descriptors.push(UnitDescriptor::new(
                    "line",
                    format!("{}/{vector}", id.name()),
                ));
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
        self.descriptors.push(UnitDescriptor::new(
            "line.target",
            format!(
                "{}/{}-{}@{target_start}",
                id.name(),
                source_range.start(),
                source_range.end()
            ),
        ));
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
                    (base..(base + self.len))
                        .contains(&addr)
                        .then(|| addr - base)
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
    }
}
