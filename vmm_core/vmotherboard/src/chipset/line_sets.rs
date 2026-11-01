// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use chipset_device::ChipsetDevice;
use chipset_device_resources::LineSetId;
use closeable_mutex::CloseableMutex;
use inspect::InspectMut;
use state_unit::SpawnedUnit;
use state_unit::StateUnit;
use state_unit::StateUnits;
use state_unit::UnitHandle;
use std::collections::HashMap;
use std::sync::Arc;
use vmcore::line_interrupt::LineSet;
use vmcore::line_interrupt::LineSetTarget;
use vmcore::vm_task::VmTaskDriverSource;

pub struct LineSets {
    map: HashMap<LineSetId, (Arc<LineSet>, usize)>,
    pub units: Vec<SpawnedUnit<()>>,
}

impl LineSets {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            units: Vec::new(),
        }
    }

    pub fn line_set(
        &mut self,
        driver: &VmTaskDriverSource,
        units: &StateUnits,
        id: LineSetId,
    ) -> (&LineSet, &UnitHandle) {
        let (line_set, index) = self.map.entry(id.clone()).or_insert_with(|| {
            let set = Arc::new(LineSet::new());
            let unit = units
                .add(format!("lines/{}", id.name()))
                .spawn(driver.simple(), {
                    let set = set.clone();
                    async move |mut recv| {
                        while let Ok(req) = recv.recv().await {
                            req.apply(&mut LineSetUnit(&set)).await;
                        }
                    }
                })
                .unwrap();
            let index = self.units.len();
            self.units.push(unit);
            (set, index)
        });
        (line_set, self.units[*index].handle())
    }
}

#[derive(InspectMut)]
#[inspect(transparent)]
struct LineSetUnit<'a>(&'a LineSet);

impl StateUnit for LineSetUnit<'_> {
    async fn start(&mut self) {
        self.0.start();
    }

    async fn stop(&mut self) {
        self.0.stop();
    }

    async fn reset(&mut self) -> anyhow::Result<()> {
        self.0.reset();
        Ok(())
    }

    async fn save(
        &mut self,
    ) -> Result<Option<vmcore::save_restore::SavedStateBlob>, vmcore::save_restore::SaveError> {
        Ok(None)
    }

    async fn restore(
        &mut self,
        _: vmcore::save_restore::SavedStateBlob,
    ) -> Result<(), vmcore::save_restore::RestoreError> {
        Err(vmcore::save_restore::RestoreError::SavedStateNotSupported)
    }
}

pub struct LineSetTargetDevice<T> {
    device: Arc<CloseableMutex<T>>,
}

impl<T: ChipsetDevice> LineSetTargetDevice<T> {
    pub fn new(device: Arc<CloseableMutex<T>>) -> Self {
        Self { device }
    }
}

impl<T: ChipsetDevice> LineSetTarget for LineSetTargetDevice<T> {
    fn set_irq(&self, vector: u32, high: bool) {
        self.device
            .lock()
            .supports_line_interrupt_target()
            .unwrap()
            .set_irq(vector, high);
    }

    fn set_unconnected_irq(&self, vector: u32) {
        self.set_irq(vector, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chipset::backing::arc_mutex::state_unit::ArcMutexChipsetDeviceUnit;
    use chipset::ioapic::IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS;
    use chipset::ioapic::IoApicDevice;
    use chipset_device::interrupt::LineInterruptTarget;
    use chipset_device::mmio::MmioIntercept;
    use chipset_device_resources::IRQ_LINE_SET;
    use chipset_resources::ioapic::IoApicRouting;
    use mesh::payload::Protobuf;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::save_restore::AsyncSaveRestore;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateBlob;
    use vmcore::save_restore::SavedStateRoot;
    use vmcore::vm_task::SingleDriverBackend;

    #[derive(Clone, Default)]
    struct Routing {
        assertions: Arc<Mutex<Vec<u8>>>,
        target_started: Arc<AtomicBool>,
    }

    impl IoApicRouting for Routing {
        fn assert(&self, irq: u8) {
            assert!(self.target_started.load(Ordering::Relaxed));
            self.assertions.lock().push(irq);
        }

        fn set_route(&self, _: u8, _: Option<(u64, u32)>) {}
    }

    #[derive(InspectMut)]
    struct Target {
        #[inspect(skip)]
        ioapic: IoApicDevice,
        #[inspect(skip)]
        started: Arc<AtomicBool>,
    }

    impl ChipsetDevice for Target {
        fn supports_line_interrupt_target(&mut self) -> Option<&mut dyn LineInterruptTarget> {
            Some(&mut self.ioapic)
        }
    }

    impl ChangeDeviceState for Target {
        fn start(&mut self) {
            self.started.store(true, Ordering::Relaxed);
            self.ioapic.start();
        }

        async fn stop(&mut self) {
            self.ioapic.stop().await;
            self.started.store(false, Ordering::Relaxed);
        }

        async fn reset(&mut self) {
            self.ioapic.reset().await;
        }
    }

    impl AsyncSaveRestore for Target {
        type SavedState = <IoApicDevice as SaveRestore>::SavedState;

        async fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            SaveRestore::save(&mut self.ioapic)
        }

        async fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            SaveRestore::restore(&mut self.ioapic, state)
        }
    }

    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "line_lifecycle_test")]
    struct ProducerState(bool);

    #[derive(InspectMut)]
    struct Producer {
        level: bool,
        #[inspect(skip)]
        line: LineInterrupt,
        #[inspect(skip)]
        restore_level: bool,
        #[inspect(skip)]
        assertions: Arc<Mutex<Vec<u8>>>,
        #[inspect(skip)]
        expected_at_start: Arc<AtomicUsize>,
    }

    impl StateUnit for Producer {
        async fn start(&mut self) {
            // Replay must finish before asynchronous producer activity starts.
            assert_eq!(
                self.assertions.lock().len(),
                self.expected_at_start.load(Ordering::Relaxed)
            );
        }

        async fn stop(&mut self) {}

        async fn reset(&mut self) -> anyhow::Result<()> {
            self.level = false;
            self.line.set_level(false);
            Ok(())
        }

        async fn save(&mut self) -> Result<Option<SavedStateBlob>, SaveError> {
            Ok(Some(SavedStateBlob::new(ProducerState(self.level))))
        }

        async fn restore(&mut self, state: SavedStateBlob) -> Result<(), RestoreError> {
            let _: ProducerState = state.parse()?;
            // Model an intentional device condition change across restore.
            self.level = self.restore_level;
            self.line.set_level(self.level);
            Ok(())
        }
    }

    struct Fixture {
        units: StateUnits,
        routing: Routing,
        expected_at_start: Arc<AtomicUsize>,
        _line_sets: LineSets,
        _target: SpawnedUnit<ArcMutexChipsetDeviceUnit>,
        _producers: Vec<SpawnedUnit<Producer>>,
    }

    impl Fixture {
        fn new(driver: DefaultDriver, initial: &[bool], restored: &[bool]) -> Self {
            let driver = VmTaskDriverSource::new(SingleDriverBackend::new(driver));
            let units = StateUnits::new();
            let mut line_sets = LineSets::new();
            let (set, handle) = line_sets.line_set(&driver, &units, IRQ_LINE_SET);
            let routing = Routing::default();
            let mut ioapic = IoApicDevice::new(1, Box::new(routing.clone()));
            let base = IOAPIC_DEVICE_MMIO_REGION_BASE_ADDRESS;
            ioapic.mmio_write(base, &[0x10]).unwrap();
            ioapic
                .mmio_write(base + 0x10, &0x30u32.to_ne_bytes())
                .unwrap();
            let target = Arc::new(CloseableMutex::new(Target {
                ioapic,
                started: routing.target_started.clone(),
            }));
            set.add_target(
                0..=0,
                0,
                "ioapic",
                Arc::new(LineSetTargetDevice::new(target.clone())),
            );
            let target_unit = ArcMutexChipsetDeviceUnit::new(target, false);
            let target_unit = units
                .add("ioapic")
                .dependency_of(handle)
                .spawn(driver.simple(), |recv| target_unit.run(recv))
                .unwrap();
            let expected_at_start =
                Arc::new(AtomicUsize::new(usize::from(initial.iter().any(|&v| v))));
            let mut producers: Vec<SpawnedUnit<Producer>> = Vec::new();
            for (i, (&level, &restore_level)) in initial.iter().zip(restored).enumerate() {
                let line = set.new_line(0, format!("producer{i}")).unwrap();
                line.set_level(level);
                let producer = Producer {
                    level,
                    line,
                    restore_level,
                    assertions: routing.assertions.clone(),
                    expected_at_start: expected_at_start.clone(),
                };
                let mut builder = units.add(format!("producer{i}")).depends_on(handle);
                if let Some(previous) = producers.last() {
                    builder = builder.depends_on(previous.handle());
                }
                producers.push(
                    builder
                        .spawn(driver.simple(), |recv| state_unit::run_unit(producer, recv))
                        .unwrap(),
                );
            }
            Fixture {
                units,
                routing,
                expected_at_start,
                _line_sets: line_sets,
                _target: target_unit,
                _producers: producers,
            }
        }
    }

    #[async_test]
    async fn ioapic_replays_restore_changes_only_after_target_start(driver: DefaultDriver) {
        for history in [false, true] {
            for current in [false, true] {
                let mut fixture = Fixture::new(driver.clone(), &[history], &[current]);
                fixture.units.start().await;
                fixture.units.stop().await;
                let saved = fixture.units.save().await.unwrap();
                fixture.routing.assertions.lock().clear();

                fixture.units.reset().await.unwrap();
                fixture.units.restore(saved).await.unwrap();
                assert!(fixture.routing.assertions.lock().is_empty());

                // Save-before-start must retain the unprocessed historical level.
                let saved = fixture.units.save().await.unwrap();
                fixture.units.reset().await.unwrap();
                fixture.units.restore(saved).await.unwrap();
                assert!(fixture.routing.assertions.lock().is_empty());
                let expected = usize::from(!history && current);
                fixture.expected_at_start.store(expected, Ordering::Relaxed);
                fixture.units.start().await;
                assert_eq!(fixture.routing.assertions.lock().len(), expected);
                fixture.units.stop().await;
            }
        }
    }

    #[async_test]
    async fn shared_restore_order_does_not_manufacture_an_ioapic_edge(driver: DefaultDriver) {
        for restored in [[false, true], [true, false]] {
            let mut fixture = Fixture::new(driver.clone(), &[false, true], &restored);
            fixture.units.start().await;
            fixture.units.stop().await;
            let saved = fixture.units.save().await.unwrap();
            fixture.routing.assertions.lock().clear();
            fixture.units.reset().await.unwrap();
            fixture.units.restore(saved).await.unwrap();
            assert!(fixture.routing.assertions.lock().is_empty());
            fixture.expected_at_start.store(0, Ordering::Relaxed);
            fixture.units.start().await;
            assert!(fixture.routing.assertions.lock().is_empty());
            fixture.units.stop().await;
        }
    }

    #[async_test]
    async fn pic_reset_restore_does_not_replay_a_stale_ready_output() {
        use chipset::pic::DualPic;
        use chipset_device::pio::ExternallyManagedPortIoIntercepts;

        #[derive(Default)]
        struct LintTarget(Mutex<Vec<bool>>);

        impl LineSetTarget for LintTarget {
            fn set_irq(&self, _: u32, high: bool) {
                self.0.lock().push(high);
            }
        }

        let lint = LineSet::new();
        let target = Arc::new(LintTarget::default());
        lint.add_target(0..=0, 0, "lint", target.clone());
        let pic = Arc::new(CloseableMutex::new(DualPic::new(
            lint.new_line(0, "pic ready").unwrap(),
            &mut ExternallyManagedPortIoIntercepts,
        )));
        let saved = SaveRestore::save(&mut *pic.lock()).unwrap();
        let irq = LineSet::new();
        irq.add_target(
            0..=1,
            0,
            "pic",
            Arc::new(LineSetTargetDevice::new(pic.clone())),
        );
        let producer = irq.new_line(1, "device").unwrap();
        producer.set_level(false);

        lint.start();
        pic.lock().start();
        irq.start();
        producer.set_level(true);
        assert!(target.0.lock().contains(&true));

        irq.stop();
        pic.clone().close().stop().await;
        lint.stop();
        lint.reset();
        pic.clone().close().reset().await;
        irq.reset();
        producer.set_level(false);
        SaveRestore::restore(&mut *pic.lock(), saved).unwrap();
        target.0.lock().clear();

        lint.start();
        pic.lock().start();
        irq.start();
        assert!(!target.0.lock().contains(&true));
        producer.set_level(true);
        assert!(target.0.lock().contains(&true));
        irq.stop();
        pic.clone().close().stop().await;
        lint.stop();
    }

    #[async_test]
    async fn queued_generation_id_notifications_reach_the_started_gpe_target() {
        struct GpeLatch(AtomicBool);

        impl LineSetTarget for GpeLatch {
            fn set_irq(&self, _: u32, high: bool) {
                if high {
                    self.0.store(true, Ordering::Relaxed);
                }
            }
        }

        let set = LineSet::new();
        let target = Arc::new(GpeLatch(AtomicBool::new(false)));
        set.add_target(0..=0, 0, "gpe", target.clone());
        let (send, recv) = mesh::channel();
        let mut genid = generation_id::GenerationId::new(
            [0; 16],
            generation_id::GenerationIdRuntimeDeps {
                gm: guestmem::GuestMemory::allocate(64),
                generation_id_recv: recv,
                notify_interrupt: set.new_line(0, "genid").unwrap(),
            },
        );
        let mut unit = LineSetUnit(&set);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        for id in [1, 2] {
            send.send([id; 16]);
            assert!(!target.0.load(Ordering::Relaxed));
            unit.start().await;
            assert!(!target.0.load(Ordering::Relaxed));
            genid.poll(&mut cx);
            assert!(target.0.load(Ordering::Relaxed));
            unit.stop().await;
            target.0.store(false, Ordering::Relaxed);
        }
    }
}
