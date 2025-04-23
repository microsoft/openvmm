// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! APIC tests.

use crate::prelude::*;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;
use x86defs::apic::APIC_BASE_ADDRESS;
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

enum ApicMode {
    XApic(u32),
    X2Apic,
}

impl ApicMode {
    fn init(&self, s: &mut Scope<'_, '_>) {
        match self {
            &ApicMode::XApic(base) => {
                let mut msr = ApicBase::from(s.read_msr(x86defs::X86X_MSR_APIC_BASE).unwrap());
                msr.set_x2apic(false);
                msr.set_enable(false);
                s.write_msr(x86defs::X86X_MSR_APIC_BASE, msr.into())
                    .unwrap();
                msr.set_enable(true);
                msr.set_base_page(base >> 12);
                s.write_msr(x86defs::X86X_MSR_APIC_BASE, msr.into())
                    .unwrap();
            }
            ApicMode::X2Apic => {
                let msr = ApicBase::from(s.read_msr(x86defs::X86X_MSR_APIC_BASE).unwrap());
                s.write_msr(
                    x86defs::X86X_MSR_APIC_BASE,
                    msr.with_x2apic(true).with_enable(true).into(),
                )
                .unwrap();
            }
        }

        // Enable software APIC.
        self.write(
            s,
            x86defs::apic::ApicRegister::SVR,
            u32::from(
                x86defs::apic::Svr::new()
                    .with_enable(true)
                    .with_vector(0xff),
            )
            .into(),
        );
    }

    fn read(&self, s: &mut Scope<'_, '_>, reg: x86defs::apic::ApicRegister) -> u32 {
        match self {
            &ApicMode::XApic(base) => {
                let p = (base + reg.0 as u32 * 0x10) as *const u32;
                unsafe { p.read_volatile() }
            }
            ApicMode::X2Apic => s.read_msr(reg.x2apic_msr()).unwrap() as u32,
        }
    }

    fn write(&self, s: &mut Scope<'_, '_>, reg: x86defs::apic::ApicRegister, value: u32) {
        match self {
            &ApicMode::XApic(base) => {
                let p = (base + reg.0 as u32 * 0x10) as *mut u32;
                unsafe { p.write_volatile(value) }
            }
            ApicMode::X2Apic => s.write_msr(reg.x2apic_msr(), value.into()).unwrap(),
        }
    }
}

#[tmk_test]
fn self_ipi_x2apic(t: TestContext<'_>) {
    self_ipi(t, ApicMode::X2Apic);
}

#[tmk_test]
fn self_ipi_xapic(t: TestContext<'_>) {
    self_ipi(t, ApicMode::XApic(APIC_BASE_ADDRESS));
}

#[tmk_test]
fn self_ipi_xapic_moved(t: TestContext<'_>) {
    self_ipi(t, ApicMode::XApic(APIC_BASE_ADDRESS + 0x1000));
}

fn self_ipi(t: TestContext<'_>, apic: ApicMode) {
    apic.init(t.scope);

    let got_interrupt = AtomicBool::new(false);
    let isr = |_: &mut IsrContext<'_>| {
        got_interrupt.store(true, Relaxed);
    };
    t.scope.subscope(|s| {
        let vector = 0x80;
        s.set_isr(vector, &isr);

        // Send self IPI.
        if let ApicMode::X2Apic = apic {
            s.write_msr(
                x86defs::apic::ApicRegister::SELF_IPI.x2apic_msr(),
                vector.into(),
            )
            .unwrap();
        } else {
            let icr = x86defs::apic::Icr::new()
                .with_destination_shorthand(x86defs::apic::DestinationShorthand::SELF.0)
                .with_vector(vector);
            let icr = u64::from(icr);
            apic.write(s, x86defs::apic::ApicRegister::ICR1, (icr >> 32) as u32);
            apic.write(s, x86defs::apic::ApicRegister::ICR0, icr as u32);
        }

        // Verify IRR.
        let irr = apic.read(s, x86defs::apic::ApicRegister::IRR4);
        assert_eq!(irr, 1);

        s.enable_interrupts();
        assert!(got_interrupt.load(Relaxed));

        // Verify IRR and ISR.
        let irr = apic.read(s, x86defs::apic::ApicRegister::IRR4);
        assert_eq!(irr, 0);
        let isr = apic.read(s, x86defs::apic::ApicRegister::ISR4);
        assert_eq!(isr, 1);

        // EOI
        apic.write(s, x86defs::apic::ApicRegister::EOI, 0);

        // Verify ISR.
        let isr = apic.read(s, x86defs::apic::ApicRegister::ISR4);
        assert_eq!(isr, 0);
    });
}

#[tmk_test]
fn apic_timer(t: TestContext<'_>) {
    let apic = ApicMode::X2Apic;
    apic.init(t.scope);

    apic.write(
        t.scope,
        x86defs::apic::ApicRegister::TIMER_DCR,
        x86defs::apic::Dcr::new().with_value_low(0b11).into(),
    );
    apic.write(t.scope, x86defs::apic::ApicRegister::TIMER_ICR, 100_000);

    // Enable the timer interrupt.
    let timer_vector = 0x40;
    apic.write(
        t.scope,
        x86defs::apic::ApicRegister::LVT_TIMER,
        x86defs::apic::Lvt::new()
            .with_vector(timer_vector)
            .with_timer_mode(x86defs::apic::TimerMode::PERIODIC.0)
            .into(),
    );

    let timer_fired = AtomicBool::new(false);
    let timer_isr = |_: &mut IsrContext<'_>| {
        log!("fired");
        timer_fired.store(true, Relaxed);
    };

    t.scope.subscope(|s| {
        s.set_isr(timer_vector, &timer_isr);
        s.enable_interrupts();

        // Wait for the timer interrupt to fire.
        for _ in 0..1_000_000 {
            if timer_fired.load(Relaxed) {
                break;
            }
            core::hint::spin_loop();
        }

        assert!(
            timer_fired.load(Relaxed),
            "APIC timer interrupt did not fire"
        );

        // Verify the current count is decreasing.
        let current_count = apic.read(s, x86defs::apic::ApicRegister::TIMER_CCR);
        log!("current count: {}", current_count);
        assert!(current_count < 100_000, "APIC timer count did not decrease");

        // Disable the timer.
        apic.write(
            s,
            x86defs::apic::ApicRegister::LVT_TIMER,
            x86defs::apic::Lvt::new()
                .with_vector(timer_vector)
                .with_masked(true)
                .into(),
        );

        // Verify the timer is disabled.
        let count1 = apic.read(s, x86defs::apic::ApicRegister::TIMER_CCR);
        let count2 = apic.read(s, x86defs::apic::ApicRegister::TIMER_CCR);
        assert_eq!(count1, count2, "APIC timer count changed after disabling");
    });
}
