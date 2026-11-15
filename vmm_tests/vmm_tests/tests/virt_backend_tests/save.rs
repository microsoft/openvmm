// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the state that a save accepts.

use super::native::backend_test;
use anyhow::Context as _;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::irqcon::DeliveryMode;
use virt::irqcon::MsiRequest;
use virt::vp::AccessVpState as _;

// KVM keeps the global interrupt flag (GIF) of an AMD vCPU only in its nested
// state, which no element of the saved state holds, so a save refuses a vCPU
// whose GIF is clear, and one whose GIF it cannot read. A new vCPU's GIF is set,
// so its save must proceed on every host. On an AMD host, this shows that KVM
// reports the flag to the save.
backend_test!(saves_a_new_vp, kvm_only, requires: []);
fn saves_a_new_vp(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    state
        .check_saveable()
        .context("the check refused a new VP's save")?;
    state.save_all().context("saving a new VP failed")?;
    Ok(())
}

// An SMI that reaches a stopped VP stays pending in KVM until the VP runs, and
// the saved state has no place for it: a restore would lose it. A save refuses
// the VP, without taking the SMI, so that the VP still has it when a capture
// rolls back.
backend_test!(refuses_a_vp_with_an_smi_pending, kvm_only, requires: []);
fn refuses_a_vp_with_an_smi_pending(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    state
        .check_saveable()
        .context("the check refused a new VP's save")?;

    partition.request_msi(
        Vtl::Vtl0,
        MsiRequest::new_x86(DeliveryMode::SMI, 0, false, 0, false),
    );

    for attempt in 1..=2 {
        anyhow::ensure!(
            state.save_all().is_err(),
            "saving a VP with an SMI pending succeeded (attempt {attempt})"
        );
        anyhow::ensure!(
            state.check_saveable().is_err(),
            "the check accepted a VP with an SMI pending (attempt {attempt})"
        );
    }
    Ok(())
}
