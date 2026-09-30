// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Shared unenlightened, single-VP fixture without mapped RAM or VP execution.

use anyhow::Context as _;
use guestmem::GuestMemory;
use pal_async::DefaultDriver;
use virt::BindProcessor;
use virt::Hypervisor;
use virt::Partition;
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
    ) -> anyhow::Result<(H::Partition, impl BindProcessor)>
    where
        H::Partition: Partition,
    {
        let proto = hv.new_partition(virt::ProtoPartitionConfig {
            processor_topology: &self.topology,
            hv_config: None,
            vmtime: &self.time_source,
            isolation: virt::ProtoPartitionIsolation::None,
            nested_virt: false,
        })?;
        let (partition, binders) = proto.build(virt::PartitionConfig {
            mem_layout: &self.memory_layout,
            guest_memory: &self.memory,
            cpuid: &[],
            vtl0_alias_map: None,
            fault_resolver: None,
        })?;
        let [binder] = binders
            .try_into()
            .ok()
            .context("backend contract tests require exactly one VP")?;
        Ok((partition, binder))
    }
}
