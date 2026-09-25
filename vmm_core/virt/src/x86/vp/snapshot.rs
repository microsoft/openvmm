// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VP state for snapshot restore: the IA32_TSC_DEADLINE state element and
//! LAPIC timer advancement across snapshot downtime.

use super::Apic;
use super::ApicRegisters;
use crate::state::HvRegisterState;
use crate::state::StateElement;
use crate::x86::X86PartitionCapabilities;
use anyhow::Context as _;
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

    /// Advances an active TSC deadline and returns the restored deadline value.
    pub fn advance_tsc_deadline(
        &mut self,
        previous_tsc: u64,
        advanced_tsc: u64,
        deadline: u64,
    ) -> u64 {
        let mut registers = ApicRegisters::from_array(self.registers);
        let timer_mode = (registers.lvt_timer >> 17) & 0x3;
        let deadline_distance = deadline.wrapping_sub(previous_tsc) as i64;
        let elapsed_ticks = advanced_tsc - previous_tsc;
        if deadline != 0
            && timer_mode == 2
            && (deadline_distance <= 0 || deadline_distance as u64 <= elapsed_ticks)
        {
            self.queue_timer_interrupt(&mut registers);
            self.registers = *registers.as_array();
            0
        } else {
            deadline
        }
    }

    /// Advances the LAPIC timer by host downtime using the interrupt clock.
    pub fn advance_timer(
        &mut self,
        duration: std::time::Duration,
        frequency_hz: u64,
    ) -> anyhow::Result<()> {
        let registers = ApicRegisters::from_array(self.registers);
        let mut registers = registers;
        if registers.timer_ccr == 0 {
            return Ok(());
        }

        let timer_mode = (registers.lvt_timer >> 17) & 0x3;
        if timer_mode == 2 {
            // TSC-deadline mode is handled through IA32_TSC_DEADLINE.
            return Ok(());
        }
        let divider_shift = match registers.timer_dcr & 0xb {
            0xb => 0,
            0 => 1,
            1 => 2,
            2 => 3,
            3 => 4,
            8 => 5,
            9 => 6,
            0xa => 7,
            _ => unreachable!(),
        };
        let elapsed_ticks = duration
            .as_nanos()
            .checked_mul(u128::from(frequency_hz))
            .context("LAPIC timer downtime adjustment overflows")?
            / 1_000_000_000
            / (1_u128 << divider_shift);

        if elapsed_ticks >= u128::from(registers.timer_ccr) {
            self.queue_timer_interrupt(&mut registers);
            if timer_mode == 1 && registers.timer_icr != 0 {
                let remaining = elapsed_ticks - u128::from(registers.timer_ccr);
                let period = u128::from(registers.timer_icr);
                registers.timer_ccr = (period - remaining % period) as u32;
            } else {
                registers.timer_ccr = 0;
            }
        } else {
            registers.timer_ccr -= elapsed_ticks as u32;
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
    use std::time::Duration;
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

    #[test]
    fn snapshot_downtime_expires_one_shot_once() {
        let mut apic = apic(TIMER_VECTOR, 100, 50);

        apic.advance_timer(Duration::from_nanos(50), 1_000_000_000)
            .unwrap();

        assert_eq!(apic.registers().timer_ccr, 0);
        assert!(timer_pending(&apic));

        apic.advance_timer(Duration::from_secs(1), 1_000_000_000)
            .unwrap();
        assert!(timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_does_not_queue_masked_one_shot() {
        let mut apic = apic(TIMER_VECTOR | TIMER_MASKED, 100, 50);

        apic.advance_timer(Duration::from_nanos(50), 1_000_000_000)
            .unwrap();

        assert_eq!(apic.registers().timer_ccr, 0);
        assert!(!timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_coalesces_periods_and_preserves_phase() {
        let mut apic = apic(TIMER_VECTOR | TIMER_PERIODIC, 100, 25);

        apic.advance_timer(Duration::from_nanos(250), 1_000_000_000)
            .unwrap();

        assert_eq!(apic.registers().timer_ccr, 75);
        assert!(timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_preserves_near_expiry_timer() {
        let mut apic = apic(TIMER_VECTOR, 100, 50);

        apic.advance_timer(Duration::from_nanos(49), 1_000_000_000)
            .unwrap();

        assert_eq!(apic.registers().timer_ccr, 1);
        assert!(!timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_rejects_lapic_tick_overflow() {
        let mut apic = apic(TIMER_VECTOR, 100, 50);

        let error = apic.advance_timer(Duration::MAX, u64::MAX).unwrap_err();

        assert!(error.to_string().contains("overflows"));
    }

    #[test]
    fn snapshot_downtime_expires_tsc_deadline_once() {
        let mut apic = apic(TIMER_VECTOR | TIMER_TSC_DEADLINE, 0, 0);

        let deadline = apic.advance_tsc_deadline(1_000, 2_000, 2_000);

        assert_eq!(deadline, 0);
        assert!(timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_expires_already_overdue_tsc_deadline() {
        let mut apic = apic(TIMER_VECTOR | TIMER_TSC_DEADLINE, 0, 0);

        let deadline = apic.advance_tsc_deadline(1_000, 2_000, 500);

        assert_eq!(deadline, 0);
        assert!(timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_preserves_wrapped_future_tsc_deadline() {
        let mut apic = apic(TIMER_VECTOR | TIMER_TSC_DEADLINE, 0, 0);

        let deadline = apic.advance_tsc_deadline(u64::MAX - 1_000, u64::MAX - 900, 500);

        assert_eq!(deadline, 500);
        assert!(!timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_drops_masked_tsc_deadline() {
        let mut apic = apic(TIMER_VECTOR | TIMER_TSC_DEADLINE | TIMER_MASKED, 0, 0);

        let deadline = apic.advance_tsc_deadline(1_000, 2_000, 2_000);

        assert_eq!(deadline, 0);
        assert!(!timer_pending(&apic));
    }

    #[test]
    fn snapshot_downtime_ignores_deadline_outside_deadline_mode() {
        let mut apic = apic(TIMER_VECTOR, 0, 0);

        let deadline = apic.advance_tsc_deadline(1_000, 2_000, 1_500);

        assert_eq!(deadline, 1_500);
        assert!(!timer_pending(&apic));
    }
}
