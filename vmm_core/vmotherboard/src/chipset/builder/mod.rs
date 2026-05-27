// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exports [`ChipsetBuilder`].

mod errors;

use self::errors::ChipsetBuilderError;
use self::errors::ErrorListExt;
use self::errors::FinalChipsetBuilderError;
use super::backing::arc_mutex::device::ArcMutexChipsetDeviceBuilder;
use super::backing::arc_mutex::pci::BusResolverWeakMutexPci;
use super::backing::arc_mutex::pci::BusResolverWeakMutexPcie;
use super::backing::arc_mutex::pci::RegisterWeakMutexPci;
use super::backing::arc_mutex::pci::RegisterWeakMutexPcie;
use super::backing::arc_mutex::pci::WeakMutexPciEntry;
use super::backing::arc_mutex::pci::WeakMutexPcieDeviceEntry;
use super::backing::arc_mutex::services::ArcMutexChipsetServices;
use super::backing::arc_mutex::state_unit::ArcMutexChipsetDeviceUnit;
use crate::BusId;
use crate::BusIdPci;
use crate::BusIdPcieDownstreamPort;
use crate::BusIdPcieEnumerator;
use crate::DebugEventHandler;
use crate::VmmChipsetDevice;
use crate::chipset::Chipset;
use crate::chipset::io_ranges::IoRanges;
use anyhow::Context as _;
use arc_cyclic_builder::ArcCyclicBuilderExt as _;
use chipset_device::ChipsetDevice;
use chipset_device::mmio::RegisterMmioIntercept;
use chipset_device_resources::LineSetId;
use closeable_mutex::CloseableMutex;
use pal_async::task::Spawn;
use pal_async::task::Task;
use parking_lot::Mutex;
use state_unit::SpawnedUnit;
use state_unit::StateUnits;
use state_unit::UnitHandle;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::sync::Weak;
use vmcore::line_interrupt::LineSetTarget;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vmtime::VmTimeSource;

/// A (type erased) bundle of state unit handles for added devices.
pub struct ChipsetDevices {
    chipset_unit: UnitHandle,
    _chipset_task: Task<()>,
    _arc_mutex_device_units: Vec<SpawnedUnit<ArcMutexChipsetDeviceUnit>>,
    _line_set_units: Vec<SpawnedUnit<()>>,
    mmio_ranges: IoRanges<u64>,
}

impl ChipsetDevices {
    /// The root chipset unit handle.
    ///
    /// All devices that have MMIO, PIO, or PCI callbacks have a "stop after"
    /// dependency on this handle.
    pub fn chipset_unit(&self) -> &UnitHandle {
        &self.chipset_unit
    }

    /// Adds a dynamically managed device to the chipset at runtime.
    pub async fn add_dyn_device<T: VmmChipsetDevice>(
        &self,
        driver_source: &VmTaskDriverSource,
        units: &StateUnits,
        name: impl Into<Arc<str>>,
        f: impl AsyncFnOnce(&mut (dyn RegisterMmioIntercept + Send)) -> anyhow::Result<T>,
    ) -> anyhow::Result<(DynamicDeviceUnit, Arc<CloseableMutex<T>>)> {
        let name = name.into();
        let arc_builder = Arc::<CloseableMutex<T>>::new_cyclic_builder();
        let device = f(
            &mut super::backing::arc_mutex::services::register_mmio_for_device(
                name.clone(),
                arc_builder.weak(),
                self.mmio_ranges.clone(),
            ),
        )
        .await?;
        let device = arc_builder.build(CloseableMutex::new(device));
        let device_unit = ArcMutexChipsetDeviceUnit::new(device.clone(), false);
        let builder = units.add(name).dependency_of(self.chipset_unit());
        let unit = builder
            .spawn(driver_source.simple(), |recv| device_unit.run(recv))
            .context("name in use")?;

        Ok((DynamicDeviceUnit(unit), device))
    }
}

/// A unit handle for a dynamically managed device.
pub struct DynamicDeviceUnit(SpawnedUnit<ArcMutexChipsetDeviceUnit>);

impl DynamicDeviceUnit {
    /// Removes and drops the dynamically managed device.
    pub async fn remove(self) {
        self.0.remove().await;
    }
}

#[derive(Default)]
pub(crate) struct BusResolver {
    pci: BusResolverWeakMutexPci,
    pcie: BusResolverWeakMutexPcie,
}

/// Mutable state behind the builder's interior mutex.
pub(crate) struct ChipsetBuilderInner {
    pub(crate) vm_chipset: Chipset,
    pub(crate) bus_resolver: BusResolver,
    pub(crate) line_sets: super::line_sets::LineSets,
    pub(crate) arc_mutex_device_units: Vec<SpawnedUnit<ArcMutexChipsetDeviceUnit>>,
    chipset_recv: mesh::Receiver<state_unit::StateRequest>,
}

/// A builder for [`Chipset`].
///
/// Methods on this type use interior mutability so that multiple devices
/// can be constructed concurrently via `arc_mutex_device().try_add_async()`.
pub struct ChipsetBuilder<'a> {
    pub(crate) inner: Mutex<ChipsetBuilderInner>,

    // External runtime dependencies (shared, read-only)
    pub(crate) units: &'a StateUnits,
    pub(crate) driver_source: &'a VmTaskDriverSource,
    pub(crate) vmtime: &'a VmTimeSource,
    pub(crate) vmtime_unit: &'a UnitHandle,

    // Root chipset state-unit
    pub(crate) chipset_unit: UnitHandle,
}

impl<'a> ChipsetBuilder<'a> {
    pub(crate) fn new(
        driver_source: &'a VmTaskDriverSource,
        units: &'a StateUnits,
        debug_event_handler: Arc<dyn DebugEventHandler>,
        vmtime: &'a VmTimeSource,
        vmtime_unit: &'a UnitHandle,
        trace_unknown_pio: bool,
        trace_unknown_mmio: bool,
        fallback_mmio_device: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
    ) -> Self {
        let (send, chipset_recv) = mesh::channel();
        let chipset_unit = units.add("chipset").build(send).unwrap();

        Self {
            inner: Mutex::new(ChipsetBuilderInner {
                vm_chipset: Chipset {
                    mmio_ranges: IoRanges::new(trace_unknown_mmio, fallback_mmio_device),
                    pio_ranges: IoRanges::new(trace_unknown_pio, None),

                    pic: None,
                    eoi_handler: None,
                    debug_event_handler,
                },

                bus_resolver: BusResolver::default(),
                line_sets: super::line_sets::LineSets::new(),
                arc_mutex_device_units: Vec::new(),
                chipset_recv,
            }),

            units,
            driver_source,
            vmtime,
            vmtime_unit,

            chipset_unit,
        }
    }

    pub(crate) fn register_weak_mutex_pci_bus(
        &self,
        bus_id: BusIdPci,
        bus: Box<dyn RegisterWeakMutexPci>,
    ) {
        let mut inner = self.inner.lock();
        let existing = inner.bus_resolver.pci.buses.insert(bus_id.clone(), bus);
        assert!(
            existing.is_none(),
            "shouldn't be possible to have duplicate bus IDs: {:?}",
            bus_id
        )
    }

    pub(crate) fn register_weak_mutex_pci_device(
        &self,
        bus_id: BusIdPci,
        bdf: (u8, u8, u8),
        name: Arc<str>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    ) {
        self.inner
            .lock()
            .bus_resolver
            .pci
            .devices
            .entry(bus_id)
            .or_default()
            .push(WeakMutexPciEntry { bdf, name, dev });
    }

    /// Register a PCIe enumerator (ex. root complex or switch), and all of
    /// it's downstream ports.
    pub fn register_weak_mutex_pcie_enumerator(
        &self,
        bus_id: BusIdPcieEnumerator,
        enumerator: Box<dyn RegisterWeakMutexPcie>,
    ) {
        let downstream_ports = enumerator.downstream_ports();
        let mut inner = self.inner.lock();
        let existing = inner
            .bus_resolver
            .pcie
            .enumerators
            .insert(bus_id.clone(), enumerator);
        assert!(
            existing.is_none(),
            "duplicate pcie enumerator ID: {:?}",
            bus_id
        );

        for port_info in downstream_ports {
            let existing = inner.bus_resolver.pcie.ports.insert(
                BusId::new(&port_info.name),
                (port_info.port_number, bus_id.clone()),
            );
            assert!(
                existing.is_none(),
                "duplicate pcie port ID: {:?}",
                port_info.name
            );
        }
    }

    pub(crate) fn register_weak_mutex_pcie_device(
        &self,
        bus_id_port: BusIdPcieDownstreamPort,
        name: Arc<str>,
        dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
    ) {
        self.inner
            .lock()
            .bus_resolver
            .pcie
            .devices
            .push(WeakMutexPcieDeviceEntry {
                bus_id_port,
                name,
                dev,
            });
    }

    /// Add a new [`ChipsetDevice`](chipset_device::ChipsetDevice) to the
    /// chipset. **`dev_name` must be unique!**
    pub fn arc_mutex_device<'b, T: VmmChipsetDevice>(
        &'b self,
        dev_name: impl Into<Arc<str>>,
    ) -> ArcMutexChipsetDeviceBuilder<'b, 'a, T> {
        ArcMutexChipsetDeviceBuilder::new(dev_name.into(), |dev, name| {
            ArcMutexChipsetServices::new(self, dev.clone(), name)
        })
    }

    /// Wrap up device construction, returning the completed chipset and devices
    pub fn build(self) -> Result<(Arc<Chipset>, ChipsetDevices), FinalChipsetBuilderError> {
        let mut inner = self.inner.into_inner();
        let mut errs = None;

        for conflict in (inner.vm_chipset.mmio_ranges).take_static_registration_conflicts() {
            errs.append(ChipsetBuilderError::MmioConflict(conflict));
        }

        for conflict in (inner.vm_chipset.pio_ranges).take_static_registration_conflicts() {
            errs.append(ChipsetBuilderError::PioConflict(conflict));
        }

        {
            let BusResolver { pci, pcie } = inner.bus_resolver;

            match pci.resolve() {
                Ok(()) => {}
                Err(conflicts) => {
                    for conflict in conflicts {
                        errs.append(ChipsetBuilderError::PciConflict(conflict));
                    }
                }
            }

            match pcie.resolve() {
                Ok(()) => {}
                Err(conflicts) => {
                    for conflict in conflicts {
                        errs.append(ChipsetBuilderError::PcieConflict(conflict));
                    }
                }
            }
        }

        if let Some(err) = errs {
            return Err(FinalChipsetBuilderError(err));
        }

        let mmio_ranges = inner.vm_chipset.mmio_ranges.clone();

        // Spawn a task for the chipset unit.
        let vm_chipset = Arc::new(inner.vm_chipset);
        let chipset_task = self.driver_source.simple().spawn("chipset-unit", {
            let vm_chipset = vm_chipset.clone();
            let mut recv = inner.chipset_recv;
            async move {
                while let Ok(req) = recv.recv().await {
                    req.apply(&mut chipset_unit::ChipsetUnit(&vm_chipset)).await;
                }
            }
        });

        let devices = ChipsetDevices {
            chipset_unit: self.chipset_unit,
            _chipset_task: chipset_task,
            _arc_mutex_device_units: inner.arc_mutex_device_units,
            _line_set_units: inner.line_sets.units,
            mmio_ranges,
        };

        Ok((vm_chipset, devices))
    }

    /// Add a new line set target from an external source.
    pub fn add_external_line_target(
        &self,
        id: LineSetId,
        source_range: RangeInclusive<u32>,
        target_start: u32,
        debug_label: &str,
        target: Arc<dyn LineSetTarget>,
    ) {
        let mut inner = self.inner.lock();
        let (line_set, _) = inner.line_sets.line_set(self.driver_source, self.units, id);
        line_set.add_target(source_range, target_start, debug_label, target)
    }
}

mod chipset_unit {
    use crate::Chipset;
    use inspect::InspectMut;
    use state_unit::StateUnit;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SavedStateBlob;

    #[derive(InspectMut)]
    #[inspect(transparent)]
    pub struct ChipsetUnit<'a>(pub &'a Chipset);

    impl StateUnit for ChipsetUnit<'_> {
        async fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            Ok(None)
        }

        async fn restore(&mut self, _buffer: SavedStateBlob) -> Result<(), RestoreError> {
            Err(RestoreError::SavedStateNotSupported)
        }
    }
}
