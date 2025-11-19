// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exports [`ArcMutexChipsetServices`].

use self::device_range::DeviceRangeMapper;
use super::device::ArcMutexChipsetServicesFinalize;
use super::state_unit::ArcMutexChipsetDeviceUnit;
use crate::BusIdPci;
use crate::BusIdPcieDownstreamPort;
use crate::ChipsetBuilder;
use crate::VmmChipsetDevice;
use crate::chipset::io_ranges::IoRanges;
use crate::chipset::line_sets::LineSetTargetDevice;
use chipset_device::ChipsetDevice;
use chipset_device::mmio::RegisterMmioIntercept;
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
    builder: &'a mut ChipsetBuilder<'b>,
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    dev_name: Arc<str>,
    line_set_dependencies: Vec<LineSetId>,
    line_set_targets: Vec<(LineSetId, RangeInclusive<u32>, u32)>,
    line_error: Option<NewLineError>,

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

        let mut builder = {
            let mut device = device.lock();
            handle_eoi = device.supports_handle_eoi().is_some();
            acknowledge_pic_interrupt = device.supports_acknowledge_pic_interrupt().is_some();
            line_target = device
                .supports_line_interrupt_target()
                .map(|d| d.valid_lines().to_vec());

            let mut builder = self.builder.units.add(dev_name.clone());
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
            if !self.builder.try_set_eoi_handler(Some(device.clone())) {
                return Err(FinalizeError::EoiHandlerAlreadySet);
            }
        }
        if acknowledge_pic_interrupt {
            if !self.builder.try_set_pic(Some(device.clone())) {
                return Err(FinalizeError::PicHandlerAlreadySet);
            }
        }

        for id in self.line_set_dependencies {
            let (_, handle) = self.builder.line_set(id);
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
                let (line_set, handle) = self.builder.line_set(id);
                line_set.add_target(source_range, target_start, dev_name.clone(), device.clone());
                builder = builder.dependency_of(handle);
            }
        }

        let device = ArcMutexChipsetDeviceUnit::new(device.clone(), self.omit_saved_state);
        let unit = builder
            .spawn(self.builder.driver_source.simple(), |recv| device.run(recv))
            .map_err(FinalizeError::NameInUse)?;

        self.builder.register_arc_mutex_device_unit(unit);
        Ok(())
    }
}

impl<'a, 'b> ArcMutexChipsetServices<'a, 'b> {
    pub fn new(
        builder: &'a mut ChipsetBuilder<'b>,
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
        DeviceRangeMapper {
            dev: self.dev.clone(),
            dev_name: self.dev_name.clone(),
            ranges: self.builder.vm_chipset.mmio_ranges.clone(),
        }
    }

    pub fn register_pio(&self) -> DeviceRangeMapper<u16> {
        DeviceRangeMapper {
            dev: self.dev.clone(),
            dev_name: self.dev_name.clone(),
            ranges: self.builder.vm_chipset.pio_ranges.clone(),
        }
    }

    pub fn register_static_pci(&mut self, bus_id: BusIdPci, bdf: (u8, u8, u8)) {
        self.builder.register_weak_mutex_pci_device(
            bus_id,
            bdf,
            self.dev_name.clone(),
            self.dev.clone(),
        );
    }

    pub fn register_static_pcie(&mut self, bus_id: BusIdPcieDownstreamPort) {
        self.builder.register_weak_mutex_pcie_device(
            bus_id,
            self.dev_name.clone(),
            self.dev.clone(),
        );
    }

    pub fn new_line(&mut self, id: LineSetId, name: &str, vector: u32) -> LineInterrupt {
        let (line_set, _) = self.builder.line_set(id.clone());
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
    struct DeviceRange<T> {
        ranges: IoRanges<T>,
        region_name: Arc<str>,
        len: T,
        addr: Option<T>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        dev_name: Arc<str>,
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
                    if let Some(addr) = self.addr.take() {
                        self.ranges.revoke(addr)
                    }
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
}
