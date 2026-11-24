// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Contract tests of the number of VPs that a partition can have.

use anyhow::Context as _;
use pal_async::DefaultDriver;
use virt::Hypervisor;
use vm_topology::processor::TopologyBuilder;
use vm_topology::processor::x86::X2ApicState;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeKeeper;
use vmcore::vmtime::VmTimeSource;

/// The most VPs that a KVM partition with Hyper-V enlightenments can have. The
/// SynIC of each VP has a route of its own in the partition's table of 2048
/// GSIs, after the 24 lines of the I/O APIC.
const MAX_ENLIGHTENED_VPS: u32 = 2048 - 24;

/// Creates a KVM partition of `vp_count` VPs, with Hyper-V enlightenments if
/// `hv1`, without building it, so without creating its VPs.
fn new_partition(
    hv: &mut virt_kvm::Kvm,
    time_source: &VmTimeSource,
    vp_count: u32,
    hv1: bool,
) -> anyhow::Result<Result<(), virt_kvm::KvmError>> {
    let topology = TopologyBuilder::new_x86()
        .x2apic(X2ApicState::Supported)
        .build(vp_count)?;
    Ok(hv
        .new_partition(virt::ProtoPartitionConfig {
            processor_topology: &topology,
            hv_config: hv1.then_some(virt::HvConfig {
                allow_device_assignment: false,
                vtl2: None,
            }),
            vmtime: time_source,
            isolation: virt::ProtoPartitionIsolation::None,
            nested_virt: false,
            user_mode_memory_faults: false,
            lazy_memory_registration: false,
            time_abi: None,
        })
        .map(drop))
}

// A partition with Hyper-V enlightenments must refuse more VPs than it has SynIC
// routes for, with an error, before it creates any VP. Building the partition
// claimed a route for each VP, and panicked on the first that the table did not
// hold, after it had created every VP. A partition without enlightenments
// routes nothing for its VPs, so the limit is not its own.
async fn refuses_more_enlightened_vps_than_synic_routes(
    driver: DefaultDriver,
    hv: &mut virt_kvm::Kvm,
) -> anyhow::Result<()> {
    let keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
    let time_source = keeper.builder().build(&driver).await?;

    new_partition(hv, &time_source, MAX_ENLIGHTENED_VPS, true)?
        .context("a partition refused as many enlightened VPs as it has SynIC routes for")?;
    new_partition(hv, &time_source, MAX_ENLIGHTENED_VPS + 1, false)?
        .context("a partition without enlightenments refused more VPs than SynIC routes")?;
    let refused = new_partition(hv, &time_source, MAX_ENLIGHTENED_VPS + 1, true)?;
    anyhow::ensure!(
        matches!(
            refused,
            Err(virt_kvm::KvmError::TooManyVps {
                vp_count,
                max: MAX_ENLIGHTENED_VPS,
            }) if vp_count == MAX_ENLIGHTENED_VPS + 1
        ),
        "a partition did not refuse more enlightened VPs than it has SynIC routes for: {refused:?}"
    );
    Ok(())
}

// This contract creates partitions of its own, of many VPs, so it registers by
// hand, with what `backend_test!` registers for KVM with Hyper-V
// enlightenments.
petri::multitest!(vec![crate::native::kvm::test(
    "kvm::refuses_more_enlightened_vps_than_synic_routes",
    true,
    || {
        crate::native::kvm::capabilities(true)
            .as_ref()
            .map_or(true, |caps| caps.hv1)
    },
    async |driver, mut hv| refuses_more_enlightened_vps_than_synic_routes(driver, &mut hv).await,
)]);
