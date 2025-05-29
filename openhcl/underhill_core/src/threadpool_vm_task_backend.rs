// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use inspect::Inspect;
use pal_async::driver::Driver;
use underhill_threadpool::AffinitizedThreadpool;
use underhill_threadpool::RetargetableDriver;
use vmcore::vm_task::BuildVmTaskDriver;
use vmcore::vm_task::TargetedDriver;

/// A VM task driver backend backed by the threadpool.
#[derive(Debug)]
pub struct ThreadpoolBackend(AffinitizedThreadpool);

impl ThreadpoolBackend {
    pub fn new(tp: AffinitizedThreadpool) -> Self {
        Self(tp)
    }
}

impl BuildVmTaskDriver for ThreadpoolBackend {
    type Driver = ThreadpoolDriver;

    fn build(&self, name: String, target_vp: Option<u32>, run_on_target: bool) -> Self::Driver {
        let _ = name;
        ThreadpoolDriver {
            spawn_target: Target::new(&self.0, if run_on_target { target_vp } else { None }),
            io_target: Target::new(&self.0, target_vp),
        }
    }
}

#[derive(Debug, Inspect)]
pub struct ThreadpoolDriver {
    spawn_target: Target,
    io_target: Target,
}

#[derive(Debug)]
enum Target {
    Untargeted(AffinitizedThreadpool),
    Targeted(RetargetableDriver),
}

impl Inspect for Target {
    fn inspect(&self, req: inspect::Request<'_>) {
        match self {
            Target::Untargeted(_) => req.value("any"),
            Target::Targeted(driver) => req.value(driver.current_target_cpu()),
        }
    }
}

impl Target {
    fn new(tp: &AffinitizedThreadpool, vp: Option<u32>) -> Self {
        vp.map_or_else(
            || Target::Untargeted(tp.clone()),
            |vp| Target::Targeted(RetargetableDriver::new(tp.clone(), vp)),
        )
    }
}

impl TargetedDriver for ThreadpoolDriver {
    fn spawner(&self) -> &dyn pal_async::task::Spawn {
        match &self.spawn_target {
            Target::Untargeted(x) => x,
            Target::Targeted(x) => x,
        }
    }

    fn driver(&self) -> &dyn Driver {
        match &self.io_target {
            // TODO: for consistency with existing behavior, initiate IO to VP0.
            // Consider using the current initiator instead.
            Target::Untargeted(x) => x.driver(0),
            Target::Targeted(x) => x,
        }
    }

    fn retarget_vp(&self, target_vp: u32) {
        match &self.spawn_target {
            Target::Untargeted(_) => {}
            Target::Targeted(x) => x.retarget(target_vp),
        }
        match &self.io_target {
            Target::Untargeted(_) => {}
            Target::Targeted(x) => x.retarget(target_vp),
        }
    }

    fn is_target_vp_ready(&self) -> bool {
        match &self.io_target {
            Target::Untargeted(_) => true,
            Target::Targeted(x) => x.current_driver().is_affinity_set(),
        }
    }

    async fn wait_target_vp_ready(&self) {
        match &self.io_target {
            Target::Untargeted(_) => {}
            Target::Targeted(x) => x.current_driver().wait_for_affinity().await,
        }
    }
}
