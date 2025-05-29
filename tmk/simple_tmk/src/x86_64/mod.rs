// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X86-64 specific tests.

#![cfg(target_arch = "x86_64")]

mod apic;

use crate::prelude::*;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::Ordering::Relaxed;

#[tmk_test]
fn ud2(t: TestContext<'_>) {
    let recovered = AtomicBool::new(false);
    let isr = |ctx: &mut IsrContext<'_>| {
        recovered.store(true, Relaxed);
        ctx.rip += 2;
    };
    t.scope.subscope(|s| {
        s.set_isr(x86defs::Exception::INVALID_OPCODE.0, &isr);
        // SAFETY: this will cause a handled interrupt.
        unsafe {
            core::arch::asm!("ud2");
        }
    });
    assert!(recovered.load(Relaxed));
}
