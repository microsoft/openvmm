// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the INIT and SIPI that a stopped VP holds.

use super::native::backend_test;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::irqcon::DeliveryMode;
use virt::irqcon::MsiRequest;
use virt::vp;
use virt::vp::AccessVpState as _;

/// The vector of the SIPI that starts a VP at `SIPI_VECTOR << 12`.
const SIPI_VECTOR: u8 = 0x08;

/// Sends the VP, whose APIC ID is 0, an INIT or a SIPI, as another VP would
/// while this one is stopped.
fn send(partition: &impl Partition, mode: DeliveryMode, vector: u8) {
    partition.request_msi(
        Vtl::Vtl0,
        MsiRequest::new_x86(mode, 0, false, vector, false),
    );
}

/// Reads the VP's activity, which makes KVM accept what the VP holds, and then
/// its registers.
fn read(processor: &mut impl Processor) -> anyhow::Result<(vp::Activity, vp::Registers)> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let activity = state.activity()?;
    Ok((activity, state.registers()?))
}

// KVM holds an INIT that reaches a stopped VP until the VP runs, or until a
// read of its MP state accepts it, which a save does after it has read the
// registers: the saved registers would be from before the INIT and the rest of
// the saved state from after it. A capture therefore asks the VP first, and
// asking must not accept the INIT, which the capture leaves for the VP.
backend_test!(reports_an_init_that_it_has_not_accepted, kvm_only, requires: []);
fn reports_an_init_that_it_has_not_accepted(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !processor.has_pending_init_sipi()?,
        "a new VP reports an INIT or a SIPI"
    );

    // Registers that the INIT would reset.
    let reset = processor.access_state(Vtl::Vtl0).registers()?;
    let running = vp::Registers {
        rip: 0x5000,
        ..reset
    };
    processor.access_state(Vtl::Vtl0).set_registers(&running)?;

    send(partition, DeliveryMode::INIT, 0);

    for _ in 0..2 {
        anyhow::ensure!(
            processor.has_pending_init_sipi()?,
            "the VP does not report the INIT that it holds"
        );
        anyhow::ensure!(
            processor.access_state(Vtl::Vtl0).registers()? == running,
            "reporting the INIT accepted it"
        );
    }

    let (activity, registers) = read(processor)?;
    anyhow::ensure!(
        (activity.mp_state, registers.rip) == (vp::MpState::Running, reset.rip),
        "the VP did not take the INIT: {:?} at {:#x}",
        activity.mp_state,
        registers.rip
    );
    anyhow::ensure!(
        !processor.has_pending_init_sipi()?,
        "the VP reports the INIT that it accepted"
    );
    Ok(())
}

// KVM reports a SIPI only by accepting it, as it does when the VP runs, so the
// VP starts at the SIPI vector. A VP that waits for a SIPI holds none until one
// arrives.
backend_test!(reports_a_sipi_that_a_waiting_vp_has_not_accepted, kvm_only, requires: []);
fn reports_a_sipi_that_a_waiting_vp_has_not_accepted(
    partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let waiting = vp::Activity {
        mp_state: vp::MpState::WaitForSipi,
        ..Default::default()
    };
    processor.access_state(Vtl::Vtl0).set_activity(&waiting)?;
    anyhow::ensure!(
        !processor.has_pending_init_sipi()?,
        "a VP that waits for a SIPI reports one before it holds one"
    );

    send(partition, DeliveryMode::SIPI, SIPI_VECTOR);

    anyhow::ensure!(
        processor.has_pending_init_sipi()?,
        "the VP does not report the SIPI that it holds"
    );
    let (activity, registers) = read(processor)?;
    let (selector, base) = (registers.cs.selector, registers.cs.base);
    anyhow::ensure!(
        (activity.mp_state, selector, base, registers.rip)
            == (
                vp::MpState::Running,
                u16::from(SIPI_VECTOR) << 8,
                u64::from(SIPI_VECTOR) << 12,
                0
            ),
        "the VP did not start at the SIPI vector: {:?}, CS {selector:#x}:{base:#x}, RIP {:#x}",
        activity.mp_state,
        registers.rip
    );
    anyhow::ensure!(
        !processor.has_pending_init_sipi()?,
        "the VP reports the SIPI that it accepted"
    );
    Ok(())
}
