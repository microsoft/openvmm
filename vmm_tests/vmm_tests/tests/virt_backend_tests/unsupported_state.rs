// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the saved state that a restore refuses.
//!
//! A snapshot that was edited, or taken from a partition with other
//! capabilities, can hold an element that the partition that restores it lacks,
//! and a decode accepts it. The restore must then fail, with an error that names
//! the element, before it changes any state.

use super::native::backend_test;
use hvdef::Vtl;
use mesh::payload::Protobuf;
use virt::Partition;
use virt::PartitionAccessState;
use virt::Processor;
use virt::state::StateError;
use virt::vm;
use virt::vm::AccessVmState as _;
use virt::vp;
use virt::vp::AccessVpState as _;

const RAX: u64 = 0x1234_5678;

// The registers and the elements that only some partitions hold, of a saved CPU
// state, with the numbers that `VpSavedState` gives them.
#[derive(Default, Protobuf)]
#[mesh(package = "virt.x86")]
struct CpuState {
    #[mesh(1)]
    registers: Option<vp::Registers>,
    #[mesh(16)]
    spec_ctrl: Option<vp::SpecCtrl>,
    #[mesh(17)]
    umwait_control: Option<vp::UmwaitControl>,
    #[mesh(18)]
    debug_ctl: Option<vp::DebugCtl>,
    #[mesh(19)]
    misc_msrs: Option<vp::MiscMsrs>,
    #[mesh(20)]
    machine_check: Option<vp::MachineCheck>,
    #[mesh(21)]
    bndcfgs: Option<vp::Bndcfgs>,
    #[mesh(22)]
    svm_msrs: Option<vp::SvmMsrs>,
    #[mesh(23)]
    osvw: Option<vp::Osvw>,
    #[mesh(100)]
    synic: Option<vp::SyntheticMsrs>,
}

// The elements of a saved VM state, with the numbers that `VmSavedState` gives
// them.
#[derive(Default, Protobuf)]
#[mesh(package = "virt.x86")]
struct VmState {
    #[mesh(1)]
    hypercall: Option<vm::HypercallMsrs>,
    #[mesh(2)]
    reftime: Option<vm::ReferenceTime>,
    #[mesh(3)]
    reference_tsc_page: Option<vm::ReferenceTscPage>,
}

// Restoring a decoded snapshot that holds an element that the partition lacks
// fails and leaves the VP as it was. The registers come first in the restore
// order, so a restore that checked the elements one by one would have set them
// already. SynIC state is always one that the partition lacks, as the fixture
// has no Hyper-V enlightenments; the others depend on the backend and the host,
// and a case skips an element that the partition holds.
backend_test!(refuses_unsupported_cpu_state, requires: []);
fn refuses_unsupported_cpu_state(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let caps = partition.caps();
    anyhow::ensure!(
        !caps.hv1,
        "the fixture's partition has Hyper-V enlightenments"
    );
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.registers()?;

    let elements: [(&str, bool, fn(&mut CpuState)); 9] = [
        ("synic", true, |s| s.synic = Some(Default::default())),
        ("spec_ctrl", !caps.spec_ctrl, |s| {
            s.spec_ctrl = Some(Default::default())
        }),
        ("umwait_control", !caps.umwait_control, |s| {
            s.umwait_control = Some(Default::default())
        }),
        ("debug_ctl", !caps.debug_ctl, |s| {
            s.debug_ctl = Some(Default::default())
        }),
        ("misc_msrs", !caps.misc_msrs, |s| {
            s.misc_msrs = Some(Default::default())
        }),
        ("machine_check", !caps.machine_check, |s| {
            s.machine_check = Some(Default::default())
        }),
        ("bndcfgs", !caps.bndcfgs, |s| {
            s.bndcfgs = Some(Default::default())
        }),
        ("svm_msrs", !caps.svm_msrs, |s| {
            s.svm_msrs = Some(Default::default())
        }),
        ("osvw", !caps.osvw, |s| s.osvw = Some(Default::default())),
    ];
    for (element, lacked, add) in elements {
        if !lacked {
            continue;
        }
        let mut crafted = CpuState {
            registers: Some(vp::Registers { rax: RAX, ..before }),
            ..Default::default()
        };
        add(&mut crafted);
        let crafted = mesh::payload::decode::<vp::VpSavedState>(&mesh::payload::encode(crafted))?;
        match state.restore_all(&crafted) {
            Err(StateError::Unsupported { element: refused }) if refused == element => {}
            result => anyhow::bail!(
                "restoring {element} into a partition that lacks it did not refuse it: {result:?}"
            ),
        }
        anyhow::ensure!(
            state.registers()? == before,
            "the restore that refused {element} changed the registers"
        );
    }
    Ok(())
}

// The same for each element of the VM state, which a partition without Hyper-V
// enlightenments lacks.
backend_test!(refuses_unsupported_vm_state, requires: []);
fn refuses_unsupported_vm_state(
    partition: &(impl Partition + PartitionAccessState),
    _processor: &mut impl Processor,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !partition.caps().hv1 && !partition.caps().hv1_reference_tsc_page,
        "the fixture's partition has Hyper-V enlightenments"
    );
    for (element, crafted) in [
        (
            "hypercall",
            VmState {
                hypercall: Some(vm::HypercallMsrs {
                    guest_os_id: 1,
                    hypercall: 1,
                }),
                ..Default::default()
            },
        ),
        (
            "reftime",
            VmState {
                reftime: Some(vm::ReferenceTime { value: 1 }),
                ..Default::default()
            },
        ),
        (
            "reference_tsc_page",
            VmState {
                reference_tsc_page: Some(vm::ReferenceTscPage {
                    tsc_reference_page: 1,
                }),
                ..Default::default()
            },
        ),
    ] {
        let crafted = mesh::payload::decode::<vm::VmSavedState>(&mesh::payload::encode(crafted))?;
        match partition.access_state(Vtl::Vtl0).restore_all(&crafted) {
            Err(StateError::Unsupported { element: refused }) if refused == element => {}
            result => anyhow::bail!(
                "restoring {element} into a partition that lacks it did not refuse it: {result:?}"
            ),
        }
    }
    Ok(())
}
