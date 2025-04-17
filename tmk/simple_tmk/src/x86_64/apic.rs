// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! APIC tests.

use crate::prelude::*;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;
use x86defs::apic::ApicBase;

#[tmk_test]
fn enable_x2apic(t: TestContext<'_>) {
    let msr = ApicBase::from(t.scope.read_msr(x86defs::X86X_MSR_APIC_BASE).unwrap());
    log!("apic base: {:#x} {:#x?}", u64::from(msr), msr);

    assert_eq!(
        msr,
        ApicBase::new()
            .with_bsp(true)
            .with_enable(true)
            .with_base_page(x86defs::apic::APIC_BASE_PAGE)
    );

    // Should fail since reserved bits are set.
    t.scope
        .write_msr(x86defs::X86X_MSR_APIC_BASE, !0)
        .unwrap_err();

    let new_msr = msr.with_x2apic(true);
    t.scope
        .write_msr(x86defs::X86X_MSR_APIC_BASE, new_msr.into())
        .unwrap();

    assert_eq!(
        t.scope.read_msr(x86defs::X86X_MSR_APIC_BASE).unwrap(),
        new_msr.into()
    );
}

#[tmk_test]
fn self_ipi(t: TestContext<'_>) {
    let msr = ApicBase::from(t.scope.read_msr(x86defs::X86X_MSR_APIC_BASE).unwrap());
    // Enable x2apic mode.
    t.scope
        .write_msr(x86defs::X86X_MSR_APIC_BASE, msr.with_x2apic(true).into())
        .unwrap();

    // Enable software APIC.
    t.scope
        .write_msr(
            x86defs::apic::ApicRegister::SVR.x2apic_msr(),
            u32::from(
                x86defs::apic::Svr::new()
                    .with_enable(true)
                    .with_vector(0xff),
            )
            .into(),
        )
        .unwrap();

    let got_interrupt = AtomicBool::new(false);
    let isr = |_: &mut IsrContext<'_>| {
        got_interrupt.store(true, Relaxed);
    };
    t.scope.subscope(|s| {
        s.set_isr(0x80, &isr);

        // Send self IPI.
        s.write_msr(x86defs::apic::ApicRegister::SELF_IPI.x2apic_msr(), 0x80)
            .unwrap();

        // Verify IRR.
        let irr = s
            .read_msr(x86defs::apic::ApicRegister::IRR4.x2apic_msr())
            .unwrap();
        assert_eq!(irr, 1);

        s.enable_interrupts();
        assert!(got_interrupt.load(Relaxed));

        // Verify IRR and ISR.
        let irr = s
            .read_msr(x86defs::apic::ApicRegister::IRR4.x2apic_msr())
            .unwrap();
        assert_eq!(irr, 0);
        let isr = s
            .read_msr(x86defs::apic::ApicRegister::ISR4.x2apic_msr())
            .unwrap();
        assert_eq!(isr, 1);

        // EOI
        s.write_msr(x86defs::apic::ApicRegister::EOI.x2apic_msr(), 0)
            .unwrap();

        // Verify ISR.
        let isr = s
            .read_msr(x86defs::apic::ApicRegister::ISR4.x2apic_msr())
            .unwrap();
        assert_eq!(isr, 0);
    });
}
