#![cfg(target_arch = "aarch64")]
// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for ARM64.

use gic::Gic;
use tmk_core::TestContext;
use tmk_core::log;
use tmk_macros::tmk_test;

mod device_register;
mod gic;

const NUM_CPUS: usize = 1;

// TODO: qemu specific
const GICD_BASE: u64 = 0x08000000;
const GICR_BASE: u64 = 0x080a0000;

#[tmk_test]
fn gic_test(_: TestContext<'_>) {
    // Enable all exceptions.
    // SAFETY: not touching memory, inteerupt handler is set up.
    unsafe { core::arch::asm!("msr DAIFClr, #0xf", options(nomem, nostack)) };

    let mut gic = Gic::new(GICD_BASE as usize, GICR_BASE as usize, NUM_CPUS);
    gic.init_gicd();
    gic.wakeup_cpu_and_init_gicr(0);
    gic.init_icc();

    log!(
        "Initialized GIC, version {:?}, max SPI ID {}",
        gic.version(),
        gic.max_spi_id()
    );

    let irq_num = 4;
    assert!(gic.enable_sgi(irq_num, true, 0));
    assert!(gic.generate_sgi(irq_num));

    // TODO: Chnage PMR, priorities, check on pending, etc.

    unsafe { core::arch::asm!("1: wfi; b 1b") };
}
