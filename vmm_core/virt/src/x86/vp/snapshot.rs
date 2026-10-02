// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VP state for snapshot restore: the IA32_TSC_DEADLINE state element and
//! LAPIC timer advancement across snapshot downtime.

use super::Apic;
use super::ApicRegisters;
use crate::state::HvRegisterState;
use crate::state::StateElement;
use crate::time_abi::TimeAbiCode;
use crate::time_abi::TimeAbiError;
use crate::time_abi::rate;
use crate::x86::X86PartitionCapabilities;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use inspect::Inspect;
use mesh_protobuf::Protobuf;
use vm_topology::processor::x86::X86VpInfo;

impl Apic {
    fn queue_timer_interrupt(&mut self, registers: &mut ApicRegisters) {
        if registers.lvt_timer & (1 << 16) != 0 {
            return;
        }
        let vector = (registers.lvt_timer & 0xff) as usize;
        if vector < 16 {
            return;
        }
        let bank = vector / 32;
        let mask = 1 << (vector % 32);
        registers.irr[bank] |= mask;
        registers.tmr[bank] &= !mask;
        self.auto_eoi[bank] &= !mask;
    }

    /// Checks the LAPIC timer against the NVX time ABI: an armed periodic
    /// timer fails with `E_LAPIC_PERIODIC`, and TSC-deadline (or reserved)
    /// timer mode with `E_LAPIC_TSC_DEADLINE`.
    pub fn check_one_shot_timer(&self) -> Result<(), TimeAbiError> {
        let registers = ApicRegisters::from_array(self.registers);
        match (registers.lvt_timer >> 17) & 0x3 {
            1 if registers.timer_icr != 0 => Err(TimeAbiError::new(
                TimeAbiCode::LapicPeriodic,
                format!(
                    "the LAPIC timer is periodic with initial count {:#x}",
                    registers.timer_icr
                ),
            )),
            mode @ (2 | 3) => Err(TimeAbiError::new(
                TimeAbiCode::LapicTscDeadline,
                format!("the LAPIC timer is in mode {mode}, not one-shot"),
            )),
            _ => Ok(()),
        }
    }

    /// Advances a counting-mode one-shot LAPIC timer over a downtime of
    /// `downtime_ns` at `apic_hz`, by the time ABI's tick rule: if the ticks
    /// reach the current count, the count becomes 0 and the timer interrupt
    /// is queued unless the LVT is masked; otherwise the count decreases by
    /// the ticks. Fails like [`Self::check_one_shot_timer`].
    pub fn advance_one_shot_timer(
        &mut self,
        downtime_ns: u64,
        apic_hz: u64,
    ) -> Result<(), TimeAbiError> {
        self.check_one_shot_timer()?;
        let mut registers = ApicRegisters::from_array(self.registers);
        if registers.timer_ccr == 0 {
            return Ok(());
        }
        let divide = match registers.timer_dcr & 0xb {
            0xb => 1,
            0 => 2,
            1 => 4,
            2 => 8,
            3 => 16,
            8 => 32,
            9 => 64,
            0xa => 128,
            _ => unreachable!(),
        };
        let ticks = rate::lapic_ticks(downtime_ns, apic_hz, divide)
            .expect("the divide configuration is a power of two up to 128");
        if ticks >= u64::from(registers.timer_ccr) {
            registers.timer_ccr = 0;
            self.queue_timer_interrupt(&mut registers);
        } else {
            registers.timer_ccr -= ticks as u32;
        }
        self.registers = *registers.as_array();
        Ok(())
    }
}

/// IA32_TSC_DEADLINE state.
#[repr(C)]
#[derive(Default, Debug, PartialEq, Eq, Protobuf, Inspect)]
#[mesh(package = "virt.x86")]
pub struct TscDeadline {
    #[mesh(1)]
    #[inspect(hex)]
    pub value: u64,
}

impl HvRegisterState<HvX64RegisterName, 1> for TscDeadline {
    fn names(&self) -> &'static [HvX64RegisterName; 1] {
        &[HvX64RegisterName::TscDeadline]
    }

    fn get_values<'a>(&self, it: impl Iterator<Item = &'a mut HvRegisterValue>) {
        for (dest, src) in it.zip([self.value]) {
            *dest = src.into();
        }
    }

    fn set_values(&mut self, it: impl Iterator<Item = HvRegisterValue>) {
        for (src, dest) in it.zip([&mut self.value]) {
            *dest = src.as_u64();
        }
    }
}

impl StateElement<X86PartitionCapabilities, X86VpInfo> for TscDeadline {
    fn is_present(caps: &X86PartitionCapabilities) -> bool {
        caps.tsc_deadline
    }

    fn at_reset(_caps: &X86PartitionCapabilities, _vp_info: &X86VpInfo) -> Self {
        Self { value: 0 }
    }

    fn can_compare(_caps: &X86PartitionCapabilities) -> bool {
        // A deadline can expire between restoring it and reading it back.
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;
    use zerocopy::FromZeros;

    const TIMER_VECTOR: u32 = 0x40;
    const TIMER_MASKED: u32 = 1 << 16;
    const TIMER_PERIODIC: u32 = 1 << 17;
    const TIMER_TSC_DEADLINE: u32 = 2 << 17;

    fn apic(lvt_timer: u32, initial_count: u32, current_count: u32) -> Apic {
        let registers = ApicRegisters {
            lvt_timer,
            timer_icr: initial_count,
            timer_ccr: current_count,
            timer_dcr: 0xb,
            ..FromZeros::new_zeroed()
        };
        Apic {
            apic_base: 0,
            registers: *registers.as_array(),
            auto_eoi: [0; 8],
        }
    }

    fn timer_pending(apic: &Apic) -> bool {
        let registers = apic.registers();
        registers.irr[TIMER_VECTOR as usize / 32] & (1 << (TIMER_VECTOR as usize % 32)) != 0
    }

    fn with_divide(apic: Apic, dcr: u32) -> Apic {
        let mut registers = ApicRegisters::from_array(apic.registers);
        registers.timer_dcr = dcr;
        Apic {
            registers: *registers.as_array(),
            ..apic
        }
    }

    #[test]
    fn time_abi_one_shot_timer_counts_down_by_the_tick_rule() {
        // D = 1 ms at L = 200 MHz is 200,000 ticks before division.
        for (dcr, divide) in [
            (0xb, 1),
            (0, 2),
            (1, 4),
            (2, 8),
            (3, 16),
            (8, 32),
            (9, 64),
            (0xa, 128),
        ] {
            let mut timer = with_divide(apic(TIMER_VECTOR, 1_000_000, 1_000_000), dcr);
            timer
                .advance_one_shot_timer(1_000_000, 200_000_000)
                .unwrap();
            assert_eq!(timer.registers().timer_ccr, 1_000_000 - 200_000 / divide);
            assert!(!timer_pending(&timer));
        }

        // floor(999 ns * 1 GHz / 1e9) = 999 ticks.
        let mut timer = apic(TIMER_VECTOR, 1_000, 1_000);
        timer.advance_one_shot_timer(999, 1_000_000_000).unwrap();
        assert_eq!(timer.registers().timer_ccr, 1);
        assert!(!timer_pending(&timer));
    }

    #[test]
    fn time_abi_one_shot_timer_expires_once() {
        let mut timer = apic(TIMER_VECTOR, 100, 50);
        timer.advance_one_shot_timer(50, 1_000_000_000).unwrap();
        assert_eq!(timer.registers().timer_ccr, 0);
        assert!(timer_pending(&timer));
        // An expired timer stays expired; nothing else is queued.
        timer
            .advance_one_shot_timer(1_000_000_000, 1_000_000_000)
            .unwrap();
        assert_eq!(timer.registers().timer_ccr, 0);

        let mut masked = apic(TIMER_VECTOR | TIMER_MASKED, 100, 50);
        masked
            .advance_one_shot_timer(1_000_000_000, 200_000_000)
            .unwrap();
        assert_eq!(masked.registers().timer_ccr, 0);
        assert!(!timer_pending(&masked));

        let mut idle = apic(TIMER_VECTOR, 0, 0);
        idle.advance_one_shot_timer(1_000_000_000, 200_000_000)
            .unwrap();
        assert!(!timer_pending(&idle));
    }

    #[test]
    fn time_abi_rejects_periodic_and_deadline_timers() {
        let periodic = apic(TIMER_VECTOR | TIMER_PERIODIC, 100, 25);
        assert_eq!(
            periodic.check_one_shot_timer().unwrap_err().code,
            TimeAbiCode::LapicPeriodic
        );
        let mut advanced = apic(TIMER_VECTOR | TIMER_PERIODIC, 100, 25);
        assert_eq!(
            advanced
                .advance_one_shot_timer(1_000, 200_000_000)
                .unwrap_err()
                .code,
            TimeAbiCode::LapicPeriodic
        );
        // A periodic LVT with no initial count is not armed.
        apic(TIMER_VECTOR | TIMER_PERIODIC, 0, 0)
            .check_one_shot_timer()
            .unwrap();

        for lvt in [TIMER_TSC_DEADLINE, 3 << 17] {
            assert_eq!(
                apic(TIMER_VECTOR | lvt, 0, 0)
                    .check_one_shot_timer()
                    .unwrap_err()
                    .code,
                TimeAbiCode::LapicTscDeadline
            );
        }
    }
}
