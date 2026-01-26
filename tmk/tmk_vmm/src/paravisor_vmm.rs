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

use openhcl_dma_manager::AllocationVisibility;
use openhcl_dma_manager::DmaClientParameters;
use openhcl_dma_manager::LowerVtlPermissionPolicy;
use openhcl_dma_manager::OpenhclDmaManager;

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
            disable_proxy_redirect: false,
            // TODO: match openhcl defaults when TDX is supported.
            disable_lower_vtl_timer_virt: true,
        };

        let p = virt_mshv_vtl::UhProtoPartition::new(params, |_| self.state.driver.clone())?;

        let vtom = if cfg!(guest_arch = "aarch64") {
            Some((1 as u64) << (p.realm_config().ipa_width() - 1))
        } else {
            None
        };

        if cfg!(guest_arch = "aarch64") {
            p.cca_set_mem_perm(
                self.state.mmemory.as_ref().unwrap().startpa,
                self.state.mmemory.as_ref().unwrap().endpa,
            )
            .expect("failed to set CCA memory permissions");

            p.cca_set_mem_perm(
                self.state.shared_address_start,
                self.state.shared_address_start + 0x200000,
            )
            .expect("failed to set CCA memory permissions");
        }

        let m = underhill_mem::init(&underhill_mem::Init {
            processor_topology: &self.state.processor_topology,
            isolation,
            vtl0_alias_map_bit: None,
            vtom,
            mem_layout: &self.state.memory_layout,
            complete_memory_layout: &self.state.memory_layout,
            boot_init: None,
            shared_pool: &[],
            maximum_vtl: hvdef::Vtl::Vtl0,
        })
        .await?;

        let dma_manager = OpenhclDmaManager::new(
            &[],
            &self
                .state
                .memory_layout
                .ram()
                .iter()
                .map(|r| r.range)
                .collect::<Vec<_>>(),
            vtom.unwrap_or(0),
            isolation,
        )
        .expect("failed to create global dma manager");
        // Needed because if we use the same DMA manager for both below,
        // the shared manager will end up allocating some pages at the start of the address space,
        // which will conflict with the private allocations and erase some of the ELF sections
        // of the TMK.
        let shared_dma_manager = OpenhclDmaManager::new(
            &[],
            &self
                .state
                .shared_memory_layout
                .ram()
                .iter()
                .map(|r| r.range)
                .collect::<Vec<_>>(),
            vtom.unwrap_or(0),
            isolation,
        )
        .expect("failed to create global dma manager");

        let (partition, vps) = p
            .build(UhLateParams {
                gm: [
                    m.vtl0().clone(),
                    m.vtl1().cloned().unwrap_or(GuestMemory::empty()),
                ]
                .into(),
                vtl0_kernel_exec_gm: m.vtl0().clone(),
                vtl0_user_exec_gm: m.vtl0().clone(),
                #[cfg(guest_arch = "x86_64")]
                cpuid: Vec::new(),
                crash_notification_send: mesh::channel().0,
                vmtime: self.vmtime_source,
                cvm_params: Some(virt_mshv_vtl::CvmLateParams {
                    shared_gm: m.cvm_memory().unwrap().shared_gm.clone(),
                    isolated_memory_protector: m.cvm_memory().unwrap().protector.clone(),
                    shared_dma_client: shared_dma_manager.new_client(DmaClientParameters {
                        device_name: "partition-shared".into(),
                        lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                        allocation_visibility: AllocationVisibility::Private,
                        persistent_allocations: true,
                    })?,
                    private_dma_client: dma_manager.new_client(DmaClientParameters {
                        device_name: "partition-private".into(),
                        lower_vtl_policy: LowerVtlPermissionPolicy::Any,
                        allocation_visibility: AllocationVisibility::Private,
                        persistent_allocations: true,
                    })?,
                }),
                vmbus_relay: false,
            },
            self.state.shared_address_start,
            self.state.shared_virtual_address_start,
            self.state.shared_address_start_command,
            self.state.shared_virtual_address_start_command,)
            .await?;

        let partition = Arc::new(partition);

        let mut threads = Vec::new();
        let r = self
            .run(m.vtl0(), partition.caps(), test, async |_this, runner| {
                let [vp] = vps.try_into().ok().unwrap();
                threads.push(start_vp(vp, runner).await?);
                Ok(())
            })
            .await?;

        for thread in threads {
            thread.join().unwrap();
        }

        // Ensure the partition has not leaked.
        Arc::into_inner(partition).expect("partition is no longer referenced");

        Ok(r)
    }
}

async fn start_vp(
    mut vp: UhProcessorBox,
    mut runner: RunnerBuilder,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let vp_thread = std::thread::spawn(move || {
        let pool = pal_uring::IoUringPool::new("vp", 256).unwrap();
        let driver = pool.client().initiator().clone();
        pool.client().set_idle_task(async move |mut control| {

            #[cfg(guest_arch = "aarch64")]
            let vp = vp
                .bind_processor::<virt_mshv_vtl::CcaBacked>(&driver, Some(&mut control))
                .unwrap();

            #[cfg(guest_arch = "x86_64")]
            let vp = vp
                .bind_processor::<virt_mshv_vtl::HypervisorBacked>(&driver, Some(&mut control))
                .unwrap();

            runner.build(vp).unwrap().run_vp().await;
        });
        pool.run()
    });
    Ok(vp_thread)
}
