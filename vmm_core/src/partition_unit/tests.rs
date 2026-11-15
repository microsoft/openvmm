// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for partition time control across VM lifecycle transitions.

use super::*;
use futures::executor::block_on;
use parking_lot::Mutex;
use test_with_tracing::test;
use vm_topology::processor::TopologyBuilder;
use vmcore::save_restore::NoSavedState;
use vmcore::save_restore::SaveRestore;

struct Clock {
    frozen: bool,
    transitions: Vec<bool>,
}

#[derive(InspectMut)]
struct TestPartition {
    #[inspect(skip)]
    clock: Arc<Mutex<Clock>>,
}

#[async_trait]
impl VmPartition for TestPartition {
    fn initial_vp_state_source(&self) -> InitialVpStateSource {
        InitialVpStateSource::Registers
    }

    fn freeze_time(&mut self) {
        let mut clock = self.clock.lock();
        if !clock.frozen {
            clock.frozen = true;
            clock.transitions.push(true);
        }
    }

    fn thaw_time(&mut self) {
        let mut clock = self.clock.lock();
        if clock.frozen {
            clock.frozen = false;
            clock.transitions.push(false);
        }
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        assert!(self.clock.lock().frozen);
        Ok(())
    }

    fn scrub_vtl(&mut self, _vtl: Vtl) -> anyhow::Result<()> {
        self.clock.lock().frozen = true;
        Ok(())
    }

    fn accept_initial_pages(&mut self, _pages: Vec<InitialPageImport>) -> anyhow::Result<()> {
        assert!(self.clock.lock().frozen);
        Ok(())
    }
}

impl SaveRestore for TestPartition {
    type SavedState = NoSavedState;

    fn save(&mut self) -> Result<Self::SavedState, SaveError> {
        assert!(self.clock.lock().frozen);
        Ok(NoSavedState)
    }

    fn restore(&mut self, _: Self::SavedState) -> Result<(), RestoreError> {
        assert!(self.clock.lock().frozen);
        Ok(())
    }
}

fn new_runner() -> (PartitionUnitRunner, Arc<Mutex<Clock>>, Arc<Halt>) {
    let clock = Arc::new(Mutex::new(Clock {
        frozen: true,
        transitions: Vec::new(),
    }));
    let (halt, halt_recv) = Halt::new();
    let halt = Arc::new(halt);
    #[cfg(guest_arch = "x86_64")]
    let topology = TopologyBuilder::new_x86().build(1).unwrap();
    #[cfg(guest_arch = "aarch64")]
    let topology = {
        use vm_topology::processor::aarch64::Aarch64PlatformConfig;
        use vm_topology::processor::aarch64::GicMsiController;
        use vm_topology::processor::aarch64::GicVersion;

        TopologyBuilder::new_aarch64(Aarch64PlatformConfig {
            gic_distributor_base: 0x10000,
            gic_version: GicVersion::V3 {
                redistributors_base: 0x20000,
            },
            gic_msi: GicMsiController::None,
            pmu_gsiv: None,
            virt_timer_ppi: 20,
            gic_nr_irqs: 256,
        })
        .build(1)
        .unwrap()
    };
    let runner = PartitionUnitRunner {
        partition: Box::new(TestPartition {
            clock: clock.clone(),
        }),
        vp_set: VpSet::new([None, None, None], halt.clone()),
        unit_started: false,
        vp_stop_count: 0,
        needs_reset: false,
        halt_reason: None,
        halt_request_recv: halt_recv.0,
        client_notify_send: mesh::channel().0,
        req_recv: mesh::channel().1,
        topology,
        initial_regs: None,
        #[cfg(feature = "gdb")]
        debugger_state: debug::DebuggerState::new(GuestMemory::allocate(4096), None),
    };
    (runner, clock, halt)
}

#[test]
fn time_stays_frozen_across_initial_restore_and_reset() {
    block_on(async {
        let (mut runner, clock, _) = new_runner();
        let state = StateUnit::save(&mut runner).await.unwrap().unwrap();
        StateUnit::restore(&mut runner, state).await.unwrap();
        StateUnit::reset(&mut runner).await.unwrap();
        assert!(clock.lock().frozen);
        assert!(clock.lock().transitions.is_empty());
        StateUnit::start(&mut runner).await;
        assert_eq!(clock.lock().transitions, [false]);
    });
}

#[test]
fn full_stop_freezes_time_until_resume() {
    block_on(async {
        let (mut runner, clock, _) = new_runner();
        for _ in 0..2 {
            StateUnit::start(&mut runner).await;
            assert!(!clock.lock().frozen);
            StateUnit::stop(&mut runner).await;
            let state = StateUnit::save(&mut runner).await.unwrap().unwrap();
            StateUnit::restore(&mut runner, state).await.unwrap();
            StateUnit::reset(&mut runner).await.unwrap();
            assert!(clock.lock().frozen);
        }
        assert_eq!(clock.lock().transitions, [false, true, false, true]);
    });
}

#[test]
fn temporary_stops_and_debugger_halts_leave_time_running() {
    block_on(async {
        let (mut runner, clock, halt) = new_runner();
        StateUnit::start(&mut runner).await;
        runner.stop_vps().await;
        runner.stop_vps().await;
        assert!(!clock.lock().frozen);
        runner.resume_vps();
        runner.resume_vps();

        halt.halt(HaltReason::DebugBreak { vp: None });
        runner.vp_set.stop().await;
        let reason = runner.halt_request_recv.next().await.unwrap();
        runner.handle_halt(reason).await;
        assert!(!clock.lock().frozen);
        assert!(runner.clear_halt());
        assert_eq!(clock.lock().transitions, [false]);
    });
}

#[test]
fn temporary_stop_guards_do_not_override_full_stop() {
    block_on(async {
        let (mut runner, clock, _) = new_runner();
        StateUnit::start(&mut runner).await;
        runner.stop_vps().await;
        runner.stop_vps().await;
        StateUnit::stop(&mut runner).await;
        runner.resume_vps();
        assert!(clock.lock().frozen);

        StateUnit::start(&mut runner).await;
        assert!(!clock.lock().frozen);
        runner.resume_vps();
        assert_eq!(clock.lock().transitions, [false, true, false]);
    });
}

#[test]
fn temporary_scrub_thaws_time_before_restarting_vps() {
    block_on(async {
        let (mut runner, clock, _) = new_runner();
        StateUnit::start(&mut runner).await;
        runner.stop_vps().await;
        runner.partition.scrub_vtl(Vtl::Vtl2).unwrap();
        assert!(clock.lock().frozen);
        runner.resume_vps();
        assert!(!clock.lock().frozen);
    });
}
