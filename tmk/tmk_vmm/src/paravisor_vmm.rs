// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Support for running as a paravisor VMM.

#![cfg(target_os = "linux")]

use crate::run::RunContext;
use crate::run::RunnerBuilder;
use crate::run::TestResult;
use guestmem::GuestMemory;
use std::sync::Arc;
use virt::Partition;
use virt_mshv_vtl::UhLateParams;
use virt_mshv_vtl::UhPartitionNewParams;
use virt_mshv_vtl::UhProcessorBox;

impl RunContext<'_> {
    pub async fn run_paravisor_vmm(
        &mut self,
        isolation: virt::IsolationType,
        test: &crate::load::TestInfo,
    ) -> anyhow::Result<TestResult> {
        let params = UhPartitionNewParams {
            isolation,
            hide_isolation: false,
            lower_vtl_memory_layout: &self.state.memory_layout,
            topology: &self.state.processor_topology,
            cvm_cpuid_info: None,
            snp_secrets: None,
            vtom: None,
            handle_synic: true,
            no_sidecar_hotplug: false,
            use_mmio_hypercalls: false,
            intercept_debug_exceptions: false,
        };
        let p = virt_mshv_vtl::UhProtoPartition::new(params, |_| self.state.driver.clone())?;

        let m = underhill_mem::init(&underhill_mem::Init {
            processor_topology: &self.state.processor_topology,
            isolation,
            vtl0_alias_map_bit: None,
            vtom: None,
            mem_layout: &self.state.memory_layout,
            complete_memory_layout: &self.state.memory_layout,
            boot_init: None,
            shared_pool: &[],
            maximum_vtl: hvdef::Vtl::Vtl0,
        })
        .await?;

        let (partition, vps) = p
            .build(UhLateParams {
                gm: [
                    m.vtl0().clone(),
                    m.vtl1().cloned().unwrap_or(GuestMemory::empty()),
                ]
                .into(),
                #[cfg(guest_arch = "x86_64")]
                cpuid: Vec::new(),
                crash_notification_send: mesh::channel().0,
                vmtime: self.vmtime_source,
                cvm_params: None,
            })
            .await?;

        let partition = Arc::new(partition);

        self.run(m.vtl0(), partition.caps(), test, async |_this, runner| {
            let [vp] = vps.try_into().ok().unwrap();
            start_vp(vp, runner).await?;
            Ok(())
        })
        .await
    }
}

async fn start_vp(mut vp: UhProcessorBox, mut runner: RunnerBuilder) -> anyhow::Result<()> {
    std::thread::spawn(move || {
        let pool = pal_uring::IoUringPool::new("vp", 256).unwrap();
        let driver = pool.client().initiator().clone();
        pool.client().set_idle_task(async move |mut control| {
            let vp = vp
                .bind_processor::<virt_mshv_vtl::HypervisorBacked>(&driver, Some(&mut control))
                .unwrap();

            runner.build(vp).unwrap().run_vp().await;
        });
        pool.run()
    });
    Ok(())
}
