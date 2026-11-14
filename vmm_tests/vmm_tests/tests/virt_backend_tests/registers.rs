// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the register state that a restore sets.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;
use x86defs::X64_EFER_SVME;

const RAX: u64 = 0x1234_5678;

// A restored EFER.SVME gives a guest SVM, so a guest whose CPUID has none must
// not get it, whichever kernel KVM runs on: KVM refuses a guest's own write of
// the bit, but before Linux 7.3, 6.18.52, and 6.12.112 its KVM_SET_SREGS takes
// it.
backend_test!(refuses_restored_efer_svme_without_svm, kvm_only, requires: []);
fn refuses_restored_efer_svme_without_svm(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !partition.caps().nested_virt,
        "the fixture's partition offers nested virtualization"
    );
    let mut state = processor.access_state(Vtl::Vtl0);
    let before = state.registers()?;

    // The refused restore changes no register, RAX included.
    let crafted = vp::Registers {
        rax: RAX,
        efer: before.efer | X64_EFER_SVME,
        ..before
    };
    anyhow::ensure!(
        state.set_registers(&crafted).is_err(),
        "restoring EFER {:#x} succeeded for a guest without SVM",
        crafted.efer
    );
    anyhow::ensure!(
        state.registers()? == before,
        "the refused restore changed the registers"
    );

    // Without SVME, the same restore succeeds.
    let valid = vp::Registers { rax: RAX, ..before };
    state
        .set_registers(&valid)
        .context("restoring the registers without EFER.SVME failed")?;
    state.commit()?;
    let after = state.registers()?;
    anyhow::ensure!(
        (after.rax, after.efer) == (RAX, before.efer),
        "the restore without EFER.SVME did not take: RAX {:#x}, EFER {:#x}",
        after.rax,
        after.efer
    );
    Ok(())
}
