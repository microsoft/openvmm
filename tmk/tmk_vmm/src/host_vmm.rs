// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for running as a host VMM.

// UNSAFETY: needed to map guest memory.
#![expect(unsafe_code)]

use crate::run::RunContext;
use crate::run::RunnerBuilder;
use crate::run::TestResult;
use anyhow::Context as _;
use futures::executor::block_on;
use guestmem::GuestMemory;
use hvdef::Vtl;
use pal_async::DefaultDriver;
use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Context;
use std::task::Waker;
use std::time::Duration;
use virt::BindProcessor;
use virt::Hypervisor;
use virt::Partition;
use virt::PartitionAccessState;
use virt::PartitionConfig;
use virt::PartitionMemoryMapper;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::VpIndex;

#[cfg(guest_arch = "x86_64")]
mod time;

#[cfg(guest_arch = "x86_64")]
type Snapshot = time::Snapshot;
#[cfg(guest_arch = "aarch64")]
type Snapshot = ();

impl RunContext<'_> {
    pub async fn run_host_vmm<H: Hypervisor, B: BindProcessor + Send + 'static>(
        &mut self,
        mut hv: H,
        test: &crate::load::TestInfo,
    ) -> anyhow::Result<TestResult>
    where
        H::Partition: Partition + PartitionMemoryMapper + PartitionAccessState,
        for<'a> H::ProtoPartition<'a>: ProtoPartition<ProcessorBinder = B>,
    {
        let guest_memory = GuestMemory::allocate(self.state.memory_layout.end_of_ram() as usize);
        let (partition, vp) = self.build_host_partition(&mut hv, &guest_memory)?;
        if test.time_control
            && (partition.supports_time_control().is_none() || self.state.opts.disable_offloads)
        {
            return Ok(TestResult::Skipped(
                "requires native APIC and partition time control",
            ));
        }
        if test.time_control && partition.supports_reset().is_none() {
            return Ok(TestResult::Skipped("partition reset is unavailable"));
        }
        #[cfg(guest_arch = "x86_64")]
        if test.tsc_deadline && !partition.caps().tsc_deadline {
            return Ok(TestResult::Skipped("TSC deadline is unavailable"));
        }
        #[cfg(guest_arch = "aarch64")]
        if test.tsc_deadline {
            return Ok(TestResult::Skipped("TSC deadline requires x86"));
        }

        let regs = self.load_test(&guest_memory, partition.caps(), test)?;
        let result = self
            .run(&guest_memory, regs, async |this, mut runner| {
                let mut partition = partition;
                let mut vp = vp;
                let mut snapshot = None;
                loop {
                    let (returned_runner, next_snapshot) = start_vp(
                        partition.clone(),
                        vp,
                        runner,
                        snapshot,
                        this.state.driver.clone(),
                        test.time_control,
                    )
                    .await?;
                    runner = returned_runner;
                    // The VP thread and binder are gone. Release the old
                    // partition's RAM mappings before mapping a replacement.
                    drop(Arc::into_inner(partition).expect("partition is no longer referenced"));
                    let Some(saved) = next_snapshot else {
                        break;
                    };
                    (partition, vp) = this.build_host_partition(&mut hv, &guest_memory)?;
                    snapshot = Some(saved);
                }
                Ok(())
            })
            .await?;
        Ok(result)
    }

    fn build_host_partition<H: Hypervisor, B: BindProcessor + Send + 'static>(
        &self,
        hv: &mut H,
        guest_memory: &GuestMemory,
    ) -> anyhow::Result<(Arc<H::Partition>, B)>
    where
        H::Partition: Partition + PartitionMemoryMapper,
        for<'a> H::ProtoPartition<'a>: ProtoPartition<ProcessorBinder = B>,
    {
        let proto = hv
            .new_partition(ProtoPartitionConfig {
                processor_topology: &self.state.processor_topology,
                hv_config: None,
                vmtime: self.vmtime_source,
                isolation: virt::ProtoPartitionIsolation::None,
                nested_virt: false,
                #[cfg(guest_arch = "aarch64")]
                device_assignment_msi_iova_range: None,
            })
            .context("failed to create proto partition")?;

        let (partition, vps) = proto
            .build(PartitionConfig {
                mem_layout: &self.state.memory_layout,
                guest_memory,
                cpuid: &[],
                vtl0_alias_map: None,
                fault_resolver: None,
            })
            .context("failed to build partition")?;

        let partition = Arc::new(partition);

        // Map guest memory.
        for r in self.state.memory_layout.ram() {
            let range = r.range;
            // SAFETY: the guest memory is left alive as long as the partition
            // is using it.
            unsafe {
                partition
                    .memory_mapper(Vtl::Vtl0)
                    .map_range(
                        guest_memory.inner_buf().unwrap()
                            [range.start() as usize..range.end() as usize]
                            .as_ptr()
                            .cast_mut()
                            .cast(),
                        range.len() as usize,
                        range.start(),
                        true,
                        true,
                    )
                    .context("failed to map memory")
            }?;
        }

        let [vp] = vps.try_into().ok().context("TMK requires exactly one VP")?;
        Ok((partition, vp))
    }
}

trait RequestYield: Send + Sync {
    /// Forces the run_vp call to yield to the scheduler (i.e. return
    /// Poll::Pending).
    fn request_yield(&self, vp_index: VpIndex);
}

impl<T: Partition> RequestYield for T {
    fn request_yield(&self, vp_index: VpIndex) {
        self.request_yield(vp_index)
    }
}

struct VpWaker {
    partition: Weak<dyn RequestYield>,
    vp: VpIndex,
    inner: Waker,
}

impl VpWaker {
    fn new(partition: Weak<dyn RequestYield>, vp: VpIndex, waker: Waker) -> Self {
        Self {
            partition,
            vp,
            inner: waker,
        }
    }
}

impl std::task::Wake for VpWaker {
    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(partition) = self.partition.upgrade() {
            partition.request_yield(self.vp);
        }
        self.inner.wake_by_ref();
    }

    fn wake(self: Arc<Self>) {
        self.wake_by_ref()
    }
}

async fn start_vp<T: Partition + PartitionAccessState>(
    partition: Arc<T>,
    mut vp: impl 'static + BindProcessor + Send,
    mut runner: RunnerBuilder,
    snapshot: Option<Snapshot>,
    driver: DefaultDriver,
    time_test: bool,
) -> anyhow::Result<(RunnerBuilder, Option<Snapshot>)> {
    let (result_send, result_recv) = mesh::oneshot();
    let vp_thread = std::thread::spawn(move || {
        let vp_index = VpIndex::BSP;
        let result = (|| {
            let vp = vp.bind().context("failed to bind vp")?;
            let mut vp = runner.build(vp)?;
            #[cfg(guest_arch = "x86_64")]
            if let Some(snapshot) = snapshot {
                time::restore(&*partition, &mut vp, snapshot)?;
            } else if let Some(time) = partition.supports_time_control() {
                time.thaw_time();
            }
            #[cfg(guest_arch = "aarch64")]
            {
                let _ = (snapshot, time_test);
                if let Some(time) = partition.supports_time_control() {
                    time.thaw_time();
                }
            }
            let yield_partition: Arc<dyn RequestYield> = partition.clone();
            block_on(async {
                let run = async {
                    #[cfg(guest_arch = "x86_64")]
                    while let Some(request) = vp.run_once().await {
                        if let Some(snapshot) = time::checkpoint(&*partition, &mut vp, request)? {
                            return Ok(Some(snapshot));
                        }
                    }
                    #[cfg(guest_arch = "aarch64")]
                    if vp.run_once().await.is_some() {
                        anyhow::bail!("time checkpoints require x86");
                    }
                    Ok(None)
                };
                let timeout = async {
                    if !time_test {
                        std::future::pending::<()>().await;
                    }
                    pal_async::timer::PolledTimer::new(&driver)
                        .sleep(Duration::from_secs(60))
                        .await;
                    anyhow::bail!("TMK VP exceeded the 60-second host watchdog")
                };
                let run = pin!(run);
                let timeout = pin!(timeout);
                // Arm the watchdog before entering a potentially blocking VP poll.
                let mut run = pin!(futures::future::select(timeout, run));
                poll_fn(|cx| {
                    let waker = Waker::from(Arc::new(VpWaker::new(
                        Arc::downgrade(&yield_partition),
                        vp_index,
                        cx.waker().clone(),
                    )));
                    run.as_mut().poll(&mut Context::from_waker(&waker))
                })
                .await
                .factor_first()
                .0
            })
        })();
        result_send.send(result.map(|snapshot| (runner, snapshot)));
    });

    let result = result_recv.await.context("TMK VP thread terminated")?;
    vp_thread
        .join()
        .map_err(|_| anyhow::anyhow!("TMK VP thread panicked"))?;
    result
}
