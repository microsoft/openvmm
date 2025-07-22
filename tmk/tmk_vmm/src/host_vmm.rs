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
use std::future::Future;
use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Context;
use std::task::Waker;
use virt::BindProcessor;
use virt::Hypervisor;
use virt::Partition;
use virt::PartitionConfig;
use virt::PartitionMemoryMapper;
use virt::ProtoPartition;
use virt::ProtoPartitionConfig;
use virt::VpIndex;

impl RunContext<'_> {
    pub async fn run_host_vmm<H: Hypervisor>(
        &mut self,
        mut hv: H,
        test: &crate::load::TestInfo,
    ) -> anyhow::Result<TestResult>
    where
        H::Partition: Partition + PartitionMemoryMapper,
    {
        let proto = hv
            .new_partition(ProtoPartitionConfig {
                processor_topology: &self.state.processor_topology,
                hv_config: None,
                vmtime: self.vmtime_source,
                user_mode_apic: self.state.opts.disable_offloads,
                isolation: virt::IsolationType::None,
            })
            .context("failed to create proto partition")?;

        let guest_memory = GuestMemory::allocate(self.state.memory_layout.end_of_ram() as usize);

        let (partition, vps) = proto
            .build(PartitionConfig {
                mem_layout: &self.state.memory_layout,
                guest_memory: &guest_memory,
                cpuid: &[],
                vtl0_alias_map: None,
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

        let mut threads = Vec::new();
        let r = self
            .run(
                &guest_memory,
                partition.caps(),
                test,
                async |_this, runner| {
                    let [vp] = vps.try_into().ok().unwrap();
                    threads.push(start_vp(partition.clone(), vp, runner).await?);
                    Ok(())
                },
            )
            .await?;
        for thread in threads {
            thread.join().unwrap();
        }

        // Ensure the partition has not leaked.
        Arc::into_inner(partition).expect("partition is no longer referenced");

        Ok(r)
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

async fn start_vp(
    partition: Arc<dyn RequestYield>,
    mut vp: impl 'static + BindProcessor + Send,
    mut runner: RunnerBuilder,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let (bind_result_send, bind_result_recv) = mesh::oneshot();
    let vp_thread = std::thread::spawn(move || {
        let vp_index = VpIndex::BSP;
        let r = vp
            .bind()
            .context("failed to bind vp")
            .and_then(|vp| runner.build(vp));
        let (vp, r) = match r {
            Ok(vp) => (Some(vp), Ok(())),
            Err(err) => (None, Err(err)),
        };

        bind_result_send.send(r);
        let Some(mut vp) = vp else { return };
        block_on(async {
            let mut run = pin!(vp.run_vp());
            poll_fn(|cx| {
                let waker = Waker::from(Arc::new(VpWaker::new(
                    Arc::downgrade(&partition),
                    vp_index,
                    cx.waker().clone(),
                )));
                run.as_mut().poll(&mut Context::from_waker(&waker))
            })
            .await
        })
    });

    bind_result_recv.await.unwrap()?;
    Ok(vp_thread)
}
