// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to spawn VP tasks and run VPs.

use anyhow::Context;
use futures::future::try_join_all;
use pal_async::task::Spawn;
use pal_async::task::SpawnLocal;
use pal_uring::IdleControl;
use underhill_threadpool::AffinitizedThreadpool;

pub(crate) async fn spawn_vps(
    tp: &AffinitizedThreadpool,
    vps: Vec<virt_mshv_vtl::UhProcessorBox>,
    runners: Vec<vmm_core::partition_unit::VpRunner>,
    chipset: &vmm_core::vmotherboard_adapter::ChipsetPlusSynic,
    isolation: virt::IsolationType,
) -> anyhow::Result<()> {
    // Start the VP tasks on the thread pool.
    let _: Vec<()> =
        try_join_all(vps.into_iter().zip(runners).map(|(vp, runner)| {
            VpSpawner::new(vp, chipset.clone(), runner, isolation).spawn_vp(tp)
        }))
        .await?;
    Ok(())
}

/// An object to spawn and run a VP.
struct VpSpawner {
    vp: virt_mshv_vtl::UhProcessorBox,
    cpu: u32,
    chipset: vmm_core::vmotherboard_adapter::ChipsetPlusSynic,
    runner: vmm_core::partition_unit::VpRunner,
    isolation: virt::IsolationType,
}

impl VpSpawner {
    /// Creates a new spawner.
    pub fn new(
        vp: virt_mshv_vtl::UhProcessorBox,
        chipset: vmm_core::vmotherboard_adapter::ChipsetPlusSynic,
        runner: vmm_core::partition_unit::VpRunner,
        isolation: virt::IsolationType,
    ) -> Self {
        // TODO: get CPU index for VP
        let cpu = vp.vp_index().index();
        Self {
            vp,
            cpu,
            chipset,
            runner,
            isolation,
        }
    }

    /// Spawns the VP on the appropriate thread pool thread.
    pub async fn spawn_vp(self, tp: &AffinitizedThreadpool) -> anyhow::Result<()> {
        if underhill_threadpool::is_cpu_online(self.cpu)? {
            self.spawn_main_vp(tp, None, false).await
        } else {
            // The CPU is not online, so this should be a sidecar VP. Run the VP
            // remotely via the sidecar kernel.
            if self.isolation.is_isolated() {
                anyhow::bail!(
                    "cpu {} is offline, but sidecar not supported for isolated VMs",
                    self.cpu
                );
            }
            self.spawn_sidecar_vp(tp).await;
            Ok(())
        }
    }

    async fn run_backed_vp<T: virt_mshv_vtl::Backing>(
        &mut self,
        saved_state: Option<vmcore::save_restore::SavedStateBlob>,
        control: Option<&mut IdleControl>,
        save_on_cancel: bool,
    ) -> anyhow::Result<Option<vmcore::save_restore::SavedStateBlob>>
    where
        for<'a> virt_mshv_vtl::UhProcessor<'a, T>: vmcore::save_restore::ProtobufSaveRestore,
    {
        let thread = underhill_threadpool::Thread::current().unwrap();
        // TODO propagate this error back earlier. This is easiest if
        // set_idle_task is fixed to take a non-Send fn.
        let mut vp = thread.with_driver(|driver| {
            self.vp
                .bind_processor::<T>(driver, control)
                .context("failed to initialize VP")
        })?;

        if let Some(saved_state) = saved_state {
            vmcore::save_restore::ProtobufSaveRestore::restore(&mut vp, saved_state)
                .context("failed to restore saved state")?;
        }
        let state = loop {
            match self.runner.run(&mut vp, &self.chipset).await {
                Ok(()) => break None,
                Err(vmm_core::partition_unit::RunCancelled) => {
                    if save_on_cancel {
                        break Some(
                            vmcore::save_restore::ProtobufSaveRestore::save(&mut vp)
                                .context("failed to save state")?,
                        );
                    }
                }
            }
        };
        Ok(state)
    }

    async fn run_vp(
        &mut self,
        saved_state: Option<vmcore::save_restore::SavedStateBlob>,
        control: Option<&mut IdleControl>,
        save_on_cancel: bool,
    ) -> Option<vmcore::save_restore::SavedStateBlob> {
        let r = match self.isolation {
            virt::IsolationType::None | virt::IsolationType::Vbs => {
                self.run_backed_vp::<virt_mshv_vtl::HypervisorBacked>(
                    saved_state,
                    control,
                    save_on_cancel,
                )
                .await
            }
            #[cfg(guest_arch = "x86_64")]
            virt::IsolationType::Snp => {
                self.run_backed_vp::<virt_mshv_vtl::SnpBacked>(saved_state, control, save_on_cancel)
                    .await
            }
            #[cfg(guest_arch = "x86_64")]
            virt::IsolationType::Tdx => {
                self.run_backed_vp::<virt_mshv_vtl::TdxBacked>(saved_state, control, save_on_cancel)
                    .await
            }
            #[cfg(guest_arch = "aarch64")]
            _ => unimplemented!(),
        };
        match r {
            Ok(state) => state,
            Err(err) => {
                panic!(
                    "failed to run VP {vp_index}: {err:#}",
                    vp_index = self.vp.vp_index().index()
                )
            }
        }
    }

    async fn spawn_main_vp(
        mut self,
        tp: &AffinitizedThreadpool,
        mut saved_state: Option<vmcore::save_restore::SavedStateBlob>,
        was_sidecar: bool,
    ) -> anyhow::Result<()> {
        let vp_index = self.vp.vp_index();
        tp.driver(vp_index.index())
            .spawn("vp-init", async move {
                let thread = underhill_threadpool::Thread::current().unwrap();

                // If this was a relaunch of a sidecar VP, set the initial task
                // (which may have caused the sidecar VP to be removed) for
                // diagnostics purposes.
                if was_sidecar {
                    self.vp.set_sidecar_exit_due_to_task(
                        thread
                            .first_task()
                            .map_or_else(|| "<unknown>".into(), |t| t.name),
                    );
                }

                // Ensure this thread pool thread has its affinity set.
                let affinity_set = thread
                    .try_set_affinity()
                    .context("failed to set affinity")?;
                if !affinity_set {
                    anyhow::bail!("processor {} not online", vp_index.index());
                }

                thread.set_idle_task(move |mut control| async move {
                    let state = self
                        .run_vp(saved_state.take(), Some(&mut control), false)
                        .await;

                    assert!(state.is_none());
                });
                Ok(())
            })
            .await
    }

    async fn spawn_sidecar_vp(mut self, tp: &AffinitizedThreadpool) {
        let base_cpu = self.vp.sidecar_base_cpu().expect("missing sidecar");
        tp.driver(base_cpu)
            .spawn("sidecar-init", {
                let tp = tp.clone();
                async move {
                    let thread = underhill_threadpool::Thread::current().unwrap();
                    let tp = tp.clone();
                    thread
                        .spawn_local(
                            format!("sidecar-{}", self.vp.vp_index().index()),
                            async move {
                                // Cancel running the VP when the thread pool
                                // thread is spawned so that we can hotplug the
                                // processor and continue running locally.
                                let mut canceller = self.runner.canceller();
                                let offline = tp.driver(self.cpu).set_spawn_notifier(move || {
                                    canceller.cancel();
                                });

                                let saved_state = if offline {
                                    // Run until the VP is finished or cancelled. If
                                    // it is cancelled, we will hotplug the
                                    // processor and respawn the VP.
                                    let saved_state = self.run_vp(None, None, true).await;
                                    if saved_state.is_none() {
                                        // The VP is done.
                                        return;
                                    }
                                    saved_state
                                } else {
                                    // The thread has already been spawned, so
                                    // online the processor and continue without
                                    // saved state.
                                    None
                                };

                                tracing::info!(cpu = self.cpu, "onlining sidecar VP");
                                online_cpu(self.cpu).await;

                                // Respawn the VP on the new thread.
                                let vp_index = self.vp.vp_index().index();
                                if let Err(err) = self.spawn_main_vp(&tp, saved_state, true).await {
                                    panic!(
                                        "failed to spawn VP {vp_index} for onlined sidecar: {err:#}"
                                    );
                                }
                            },
                        )
                        .detach()
                }
            })
            .await;
    }
}

async fn online_cpu(cpu: u32) {
    // Spawn a thread to online the processor to avoid blocking this thread
    // (which probably wants to run another VP).
    let (send, recv) = mesh::oneshot();
    std::thread::Builder::new()
        .name(format!("online-{cpu}"))
        .spawn(move || {
            send.send({
                let _span = tracing::info_span!("online_cpu", cpu).entered();
                underhill_threadpool::set_cpu_online(cpu)
            })
        })
        .unwrap();

    if let Err(err) = recv.await.unwrap() {
        panic!("failed to online processor {cpu}: {err}");
    }
}
