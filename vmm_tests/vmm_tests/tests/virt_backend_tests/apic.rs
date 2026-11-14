// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-free contract tests of the APIC state that a backend restores.

use super::native::backend_test;
use hvdef::Vtl;
use virt::Partition;
use virt::Processor;
use virt::vp;
use virt::vp::AccessVpState as _;
use x86defs::apic::ApicBase;
use x86defs::apic::DeliveryMode;
use x86defs::apic::Lvt;
use x86defs::apic::TimerMode;

const TIMER_VECTOR: u8 = 0x40;
const INITIAL_COUNT: u32 = 1_000_000;

/// Returns `apic` with a one-shot timer that has expired: its initial count is
/// not 0, and its current count is. KVM fires such a timer as soon as it
/// restores the state, in the delivery mode of the timer's LVT entry, unless
/// the entry is masked.
fn expired_timer(apic: &vp::Apic, delivery_mode: DeliveryMode, masked: bool) -> vp::Apic {
    let mut registers = *apic.registers();
    registers.svr = 0x1ff;
    registers.lvt_timer = Lvt::new()
        .with_vector(TIMER_VECTOR)
        .with_delivery_mode(delivery_mode.0)
        .with_masked(masked)
        .with_timer_mode(TimerMode::ONE_SHOT.0)
        .into();
    registers.timer_dcr = 0xb;
    registers.timer_icr = INITIAL_COUNT;
    registers.timer_ccr = 0;
    vp::Apic::new(
        ApicBase::from(apic.apic_base).with_enable(true),
        registers,
        apic.auto_eoi,
    )
}

// KVM delivers the expiry of an unmasked timer in the delivery mode of its LVT
// entry, so most modes other than Fixed would give a restored VP an INIT, a
// startup IPI, an NMI, or an SMI, or wake it from halt, which the saved state
// does not hold; lowest priority and ExtINT, which KVM delivers as a fixed
// interrupt or ignores, would not. The guest cannot write the mode, so a save
// never holds an unmasked timer in any mode but Fixed, and the restore must
// refuse one.
backend_test!(restore_refuses_a_non_fixed_apic_timer, kvm_only, requires: []);
fn restore_refuses_a_non_fixed_apic_timer(
    _partition: &impl Partition,
    processor: &mut impl Processor,
) -> anyhow::Result<()> {
    let mut state = processor.access_state(Vtl::Vtl0);
    let reset = state.apic()?;

    let mut accepted = Vec::new();
    let mut changed = Vec::new();
    for mode in (1..=7).map(DeliveryMode) {
        if state.set_apic(&expired_timer(&reset, mode, false)).is_ok() {
            accepted.push(mode.0);
            state.set_apic(&reset)?;
        } else if state.apic()? != reset {
            changed.push(mode.0);
        }
    }
    anyhow::ensure!(
        accepted.is_empty(),
        "the restore accepted an expired timer in the delivery modes {accepted:?}"
    );
    anyhow::ensure!(
        changed.is_empty(),
        "the refused restore changed the APIC state, for the delivery modes {changed:?}"
    );

    // What a save holds restores: an expired Fixed timer, and masked timers,
    // which deliver nothing.
    state.set_apic(&expired_timer(&reset, DeliveryMode::FIXED, false))?;
    for mode in (0..=7).map(DeliveryMode) {
        state.set_apic(&expired_timer(&reset, mode, true))?;
    }
    Ok(())
}
