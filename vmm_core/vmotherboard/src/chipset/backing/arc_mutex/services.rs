// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exports [`ArcMutexChipsetServices`].

use self::device_range::DeviceRangeMapper;
use super::device::ArcMutexChipsetServicesFinalize;
use super::state_unit::ArcMutexChipsetDeviceUnit;
use crate::BusIdPci;
use crate::BusIdPcieDownstreamPort;
use crate::BusIdPcieEnumerator;
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
    builder: &'a ChipsetBuilder<'b>,
    dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    dev_name: Arc<str>,
    line_set_dependencies: Vec<LineSetId>,
    line_set_targets: Vec<(LineSetId, RangeInclusive<u32>, u32)>,
    line_error: Option<NewLineError>,

    omit_saved_state: bool,
    start_before_processors: bool,
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
            // Before stopping the device, the chipset interface must stop
            // running so that it stops issuing MMIO/PIO requests. This also
            // starts the device before the processors, which depend on the
            // chipset interface.
            if device.supports_mmio().is_some()
                || device.supports_pio().is_some()
                || device.supports_pci().is_some()
                || device.supports_handle_eoi().is_some()
                || device.supports_acknowledge_pic_interrupt().is_some()
                || self.start_before_processors
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

        let device = ArcMutexChipsetDeviceUnit::new(device.clone(), self.omit_saved_state);
        let unit = builder
            .spawn(self.builder.driver_source.simple(), |recv| device.run(recv))
            .map_err(FinalizeError::NameInUse)?;

        inner.arc_mutex_device_units.push(unit);
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

            omit_saved_state: false,
            start_before_processors: false,
        }
    }

    pub fn omit_saved_state(&mut self) {
        self.omit_saved_state = true
    }

    /// Starts the device before the processors, even if it has no MMIO, PIO,
    /// or PCI interface.
    pub fn start_before_processors(&mut self) {
        self.start_before_processors = true
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

    pub fn register_static_pcie_rciep(&mut self, enumerator_id: BusIdPcieEnumerator, devfn: u8) {
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

#[cfg(test)]
mod tests {
    use crate::ChipsetBuilder;
    use crate::DebugEventHandler;
    use chipset_device::ChipsetDevice;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use pal_async::timer::PolledTimer;
    use state_unit::StateUnit;
    use state_unit::StateUnits;
    use state_unit::run_unit;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateBlob;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;

    struct NoDebug;

    impl DebugEventHandler for NoDebug {
        fn on_debug_break(&self, _vp: Option<u32>) {}
    }

    /// A device with no MMIO, PIO, or PCI interface that records when it
    /// starts.
    #[derive(InspectMut)]
    struct PollOnlyDevice {
        #[inspect(skip)]
        started: Arc<AtomicBool>,
    }

    impl ChipsetDevice for PollOnlyDevice {}

    impl ChangeDeviceState for PollOnlyDevice {
        fn start(&mut self) {
            self.started.store(true, Ordering::SeqCst);
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl SaveRestore for PollOnlyDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: Self::SavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// A unit that delays its start, standing in for vmtime, which all
    /// devices depend on.
    #[derive(InspectMut)]
    struct SlowUnit {
        #[inspect(skip)]
        driver: DefaultDriver,
    }

    impl StateUnit for SlowUnit {
        async fn start(&mut self) {
            PolledTimer::new(&self.driver)
                .sleep(Duration::from_millis(100))
                .await;
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            Ok(None)
        }

        async fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// A unit standing in for the processors, which depend on the chipset.
    /// Records whether the device had started when this unit started.
    #[derive(InspectMut)]
    struct ProcessorsUnit {
        #[inspect(skip)]
        device_started: Arc<AtomicBool>,
        #[inspect(skip)]
        device_started_first: Arc<AtomicBool>,
    }

    impl StateUnit for ProcessorsUnit {
        async fn start(&mut self) {
            self.device_started_first
                .store(self.device_started.load(Ordering::SeqCst), Ordering::SeqCst);
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            Ok(None)
        }

        async fn restore(&mut self, _state: SavedStateBlob) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    /// Returns whether a poll-only device started before a unit that depends
    /// on the chipset unit (as the processors do).
    async fn device_starts_before_processors(
        driver: DefaultDriver,
        start_before_processors: bool,
    ) -> bool {
        let mut units = StateUnits::new();
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = keeper.builder().build(&driver).await.unwrap();
        let vmtime_unit = units
            .add("vmtime")
            .spawn(&driver, |recv| {
                run_unit(
                    SlowUnit {
                        driver: driver.clone(),
                    },
                    recv,
                )
            })
            .unwrap();

        let builder = ChipsetBuilder::new(
            &driver_source,
            &units,
            Arc::new(NoDebug),
            &vmtime,
            vmtime_unit.handle(),
            false,
            false,
            None,
        );
        let device_started = Arc::new(AtomicBool::new(false));
        builder
            .arc_mutex_device("poll-only")
            .add(|services| {
                if start_before_processors {
                    services.start_before_processors();
                }
                PollOnlyDevice {
                    started: device_started.clone(),
                }
            })
            .unwrap();
        let (_chipset, devices) = builder.build().unwrap();

        let device_started_first = Arc::new(AtomicBool::new(false));
        let _processors = units
            .add("processors")
            .depends_on(devices.chipset_unit())
            .spawn(&driver, |recv| {
                run_unit(
                    ProcessorsUnit {
                        device_started: device_started.clone(),
                        device_started_first: device_started_first.clone(),
                    },
                    recv,
                )
            })
            .unwrap();

        units.start().await;
        assert!(device_started.load(Ordering::SeqCst));
        device_started_first.load(Ordering::SeqCst)
    }

    #[async_test]
    async fn poll_only_device_not_ordered_by_default(driver: DefaultDriver) {
        assert!(!device_starts_before_processors(driver, false).await);
    }

    #[async_test]
    async fn start_before_processors_orders_poll_only_device(driver: DefaultDriver) {
        assert!(device_starts_before_processors(driver, true).await);
    }
}
