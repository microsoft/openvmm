// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared single-VP fixture with one mapped page of RAM but no VP execution.

// UNSAFETY: Mapping the fixture's RAM into the partition.
#![expect(unsafe_code)]

use anyhow::Context as _;
use guestmem::GuestMemory;
use hvdef::Vtl;
use pal_async::DefaultDriver;
use virt::BindProcessor;
use virt::Hypervisor;
use virt::Partition;
use virt::PartitionMemoryMapper;
use virt::ProtoPartition as _;
use vm_topology::memory::MemoryLayout;
use vm_topology::processor::ProcessorTopology;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::x86::X2ApicState;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use vmcore::vmtime::VmTimeSource;

pub(crate) struct Fixture {
    topology: ProcessorTopology,
    memory_layout: MemoryLayout,
    memory: GuestMemory,
    time_source: VmTimeSource,
    _keeper: VmTimeKeeper,
}

impl Fixture {
    pub(crate) async fn new(driver: DefaultDriver) -> anyhow::Result<Self> {
        let topology = TopologyBuilder::new_x86()
            .x2apic(X2ApicState::Supported)
            .build(1)?;
        let memory_layout = MemoryLayout::new(4096, &[], &[], &[], None)?;
        let memory = GuestMemory::allocate(4096);
        let keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let time_source = keeper.builder().build(&driver).await?;
        Ok(Self {
            topology,
            memory_layout,
            memory,
            time_source,
            _keeper: keeper,
        })
    }

    pub(crate) fn build<'a, H: Hypervisor>(
        &'a self,
        hv: &'a mut H,
        hv1: bool,
    ) -> anyhow::Result<(H::Partition, impl BindProcessor)>
    where
        H::Partition: Partition + PartitionMemoryMapper,
    {
        let proto = hv.new_partition(virt::ProtoPartitionConfig {
            processor_topology: &self.topology,
            hv_config: hv1.then_some(virt::HvConfig {
                allow_device_assignment: false,
                vtl2: None,
            }),
            vmtime: &self.time_source,
            isolation: virt::ProtoPartitionIsolation::None,
            nested_virt: false,
            user_mode_memory_faults: false,
            lazy_memory_registration: false,
            #[cfg(guest_arch = "x86_64")]
            time_abi: None,
            versioned_cpu_contract: false,
        })?;
        let (partition, binders) = proto.build(virt::PartitionConfig {
            mem_layout: &self.memory_layout,
            guest_memory: &self.memory,
            cpuid: &[],
            vtl0_alias_map: None,
            fault_resolver: None,
        })?;
        // An MSHV partition creates its BSP and discovers its capabilities
        // only once guest memory is attached, as at the start of a VM, so
        // attach the RAM and finalize the partition before any VP or
        // capability access.
        let ram = self
            .memory
            .inner_buf()
            .context("the fixture's RAM is not a heap allocation")?;
        // SAFETY: the fixture's RAM outlives the partition, which every
        // caller drops before the fixture.
        unsafe {
            partition.memory_mapper(Vtl::Vtl0).map_range(
                ram.as_ptr().cast_mut().cast(),
                ram.len(),
                0,
                true,
                true,
            )
        }
        .context("failed to map the fixture's RAM")?;
        partition
            .finalize_memory()
            .context("failed to finalize partition memory")?;
        let [binder] = binders
            .try_into()
            .ok()
            .context("backend contract tests require exactly one VP")?;
        Ok((partition, binder))
    }
}
