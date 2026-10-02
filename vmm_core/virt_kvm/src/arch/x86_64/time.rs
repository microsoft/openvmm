// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Software freezing of KVM clocks and native LAPIC timers.

use super::vp_state::get_msrs_state;
use super::vp_state::set_msrs_state;
use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use virt::VpIndex;
use virt::x86::vp;
use x86defs::apic::Lvt;
use x86defs::apic::TimerMode;
use zerocopy::FromZeros;

pub(crate) struct PartitionTime {
    tsc_access: TscAccess,
    frozen: Option<FrozenTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TscAccess {
    Unsupported,
    Msr,
    Offset,
    CommonClock { khz: u32 },
}

impl TscAccess {
    fn select(isolated: bool, offsets: bool, common_khz: Option<u32>) -> Self {
        if isolated {
            Self::Unsupported
        } else if offsets {
            match common_khz {
                Some(khz) => Self::CommonClock { khz },
                None => Self::Offset,
            }
        } else {
            Self::Msr
        }
    }

    fn write(&self, processor: &kvm::Processor<'_>, value: u64) -> Result<(), KvmError> {
        match self {
            Self::Offset => {
                // Sample through KVM, not userspace RDTSC: the vCPU may use
                // a different host CPU or KVM's unstable-TSC compensation.
                let current = get_msrs_state::<vp::Tsc, 1>(processor)?.value;
                let offset = processor.tsc_offset()?;
                processor.set_tsc_offset(rebase_offset(offset, current, value))?;
            }
            Self::CommonClock { .. } => {
                processor.set_tsc_offset(value.wrapping_sub(host_tsc()))?;
            }
            Self::Msr | Self::Unsupported => {
                set_msrs_state(processor, &vp::Tsc { value })?;
            }
        }
        Ok(())
    }

    fn restore(&self, processor: &kvm::Processor<'_>, value: u64) -> Result<(), KvmError> {
        if *self == Self::Msr {
            // Legacy TSC writes within a second of the previous write can
            // reuse its offset, retaining paused time. Break that matching
            // generation while VPs are stopped and LAPIC timers disarmed.
            let [unmatched, target] = legacy_tsc_restore_values(value);
            processor.set_msrs(&[
                (x86defs::X86X_MSR_TSC, unmatched),
                (x86defs::X86X_MSR_TSC, target),
            ])?;
            Ok(())
        } else {
            self.write(processor, value)
        }
    }
}

fn rebase_offset(offset: u64, current: u64, target: u64) -> u64 {
    offset.wrapping_add(target.wrapping_sub(current))
}

fn legacy_tsc_restore_values(value: u64) -> [u64; 2] {
    // Zero requests synchronization unconditionally in the legacy API.
    // Restore it one tick later instead of reusing the temporary offset.
    let target = value.max(1);
    [target ^ (1 << 63), target]
}

struct FrozenTime {
    clock_ns: u64,
    vps: Vec<FrozenVpTime>,
}

#[derive(Default)]
struct FrozenVpTime {
    tsc: u64,
    deadline: vp::TscDeadline,
    timer: Option<LapicTimer>,
    restored_stimers: Option<vp::SynicTimers>,
}

impl FrozenVpTime {
    fn set_apic(&mut self, apic: &vp::Apic) {
        self.timer = Some(LapicTimer::capture(apic));
        // Switching out of deadline mode disarms the architectural MSR,
        // just as KVM does for a live LAPIC mode change.
        if !deadline_mode(apic) {
            self.deadline = vp::TscDeadline::default();
        }
    }

    fn set_deadline(&mut self, value: vp::TscDeadline) {
        // KVM ignores deadline writes outside TSC-deadline mode.
        if self
            .timer
            .is_some_and(|timer| Lvt::from(timer.lvt).timer_mode() == TimerMode::TSC_DEADLINE.0)
        {
            self.deadline = value;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LapicTimer {
    lvt: u32,
    initial: u32,
    remaining: u32,
    divide: u32,
}

impl LapicTimer {
    fn capture(apic: &vp::Apic) -> Self {
        let regs = apic.registers();
        Self {
            lvt: regs.lvt_timer,
            initial: regs.timer_icr,
            remaining: regs.timer_ccr,
            divide: regs.timer_dcr,
        }
    }

    fn apply(&self, apic: &mut vp::Apic) {
        let mut regs = *apic.registers();
        regs.lvt_timer = self.lvt;
        regs.timer_icr = self.initial;
        regs.timer_ccr = self.remaining;
        regs.timer_dcr = self.divide;
        apic.registers = *regs.as_array();
    }

    fn disarm(apic: &mut vp::Apic) {
        let mut regs = *apic.registers();
        regs.timer_icr = 0;
        regs.timer_ccr = 0;
        apic.registers = *regs.as_array();
    }
}

fn deadline_mode(apic: &vp::Apic) -> bool {
    Lvt::from(apic.registers().lvt_timer).timer_mode() == TimerMode::TSC_DEADLINE.0
}

fn read_apic(kvm: &kvm::Processor<'_>) -> Result<vp::Apic, kvm::Error> {
    let mut base = [0];
    kvm.get_msrs(&[x86defs::X86X_MSR_APIC_BASE], &mut base)?;
    let mut page = <[u8; 1024]>::new_zeroed();
    kvm.get_lapic(&mut page)?;
    Ok(vp::Apic::new(
        base[0].into(),
        vp::ApicRegisters::from_page(&page),
        [0; 8],
    ))
}

fn write_apic(kvm: &kvm::Processor<'_>, apic: &vp::Apic) -> Result<(), kvm::Error> {
    kvm.set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, apic.apic_base)])?;
    kvm.set_lapic(&apic.registers().as_page())?;
    Ok(())
}

impl PartitionTime {
    pub(crate) fn new(
        kvm: &kvm::Partition,
        bsp: u32,
        vp_count: usize,
        isolated: bool,
        force_tsc_fallback: bool,
    ) -> Result<Self, KvmError> {
        let unsupported = Self {
            tsc_access: TscAccess::Unsupported,
            frozen: None,
        };
        if isolated {
            // KVM can accept offset writes without changing protected guest TSCs.
            return Ok(unsupported);
        }
        let offsets = kvm.vp(bsp).supports_tsc_offset()?;
        // Creation is the only point where this probe may rebase a clock before
        // exposing the interface. SET_CLOCK also refreshes KVM's masterclock
        // after vCPU creation, without needing to run guest instructions.
        kvm.set_clock_ns(0)?;
        let common_khz = if !force_tsc_fallback
            && offsets
            && kvm.get_clock_ns()?.flags & kvm::KVM_CLOCK_TSC_STABLE != 0
        {
            match kvm.vp(bsp).tsc_khz() {
                Ok(khz) => Some(khz),
                Err(kvm::Error::GetTscKhz(err)) if err as i32 == libc::EIO => {
                    tracing::info!("KVM TSC frequency unavailable; using per-VP clock capture");
                    None
                }
                Err(err) => return Err(err.into()),
            }
        } else {
            None
        };
        Ok(Self {
            tsc_access: TscAccess::select(isolated, offsets, common_khz),
            frozen: Some(FrozenTime {
                clock_ns: 0,
                vps: (0..vp_count).map(|_| FrozenVpTime::default()).collect(),
            }),
        })
    }

    pub(crate) fn is_supported(&self) -> bool {
        self.tsc_access != TscAccess::Unsupported
    }
}

struct ClockSample {
    clock: kvm::kvm_clock_data,
    host_tsc: u64,
}

fn host_tsc() -> u64 {
    // Serialize both sides of RDTSC so that clock-sampling brackets really
    // enclose the ioctl, even on processors where RDTSC is speculative.
    safe_intrinsics::cpuid(0, 0);
    let tsc = safe_intrinsics::rdtsc();
    safe_intrinsics::cpuid(0, 0);
    tsc
}

fn sample_clock(kvm: &kvm::Partition) -> Result<ClockSample, kvm::Error> {
    let mut best = None;
    let mut best_width = u64::MAX;
    for _ in 0..3 {
        let before = host_tsc();
        let clock = kvm.get_clock_ns()?;
        let after = host_tsc();
        if clock.flags & kvm::KVM_CLOCK_HOST_TSC != 0 {
            return Ok(ClockSample {
                host_tsc: clock.host_tsc,
                clock,
            });
        }
        // Guest TSC writes can disable the masterclock even on a stable host.
        // KVM then omits HOST_TSC. Use the narrowest bracket, not separately
        // sampled per-VP anchors which would introduce artificial TSC skew.
        // This fallback has up to half a bracket of sampling uncertainty.
        let width = after.wrapping_sub(before);
        if best.is_none() || width < best_width {
            best_width = width;
            best = Some(ClockSample {
                clock,
                host_tsc: before.wrapping_add(width / 2),
            });
        }
    }
    Ok(best.expect("at least one clock sample"))
}

fn thaw_offset(tsc: u64, elapsed_ns: u64, khz: u32, host_tsc: u64) -> u64 {
    let elapsed_ticks = (u128::from(elapsed_ns) * u128::from(khz) / 1_000_000) as u64;
    tsc.wrapping_add(elapsed_ticks).wrapping_sub(host_tsc)
}

impl virt::PartitionTimeControl for KvmPartition {
    fn freeze_time(&self) {
        self.inner
            .freeze_time()
            .expect("failed to freeze KVM partition time");
    }

    fn thaw_time(&self) {
        self.inner
            .thaw_time()
            .expect("failed to thaw KVM partition time");
    }
}

impl PartitionTime {
    fn freeze(
        &mut self,
        kvm: &kvm::Partition,
        apic_ids: impl Iterator<Item = u32> + Clone,
    ) -> Result<(), KvmError> {
        if self.frozen.is_some() || !self.is_supported() {
            return Ok(());
        }
        let mut vps = Vec::new();
        for apic_id in apic_ids.clone() {
            let processor = kvm.vp(apic_id);
            let mut apic = read_apic(&processor)?;
            let timer = LapicTimer::capture(&apic);
            let deadline = if deadline_mode(&apic) {
                let deadline = get_msrs_state::<vp::TscDeadline, 1>(&processor)?;
                // SET_LAPIC can rearm the previous deadline. Clear the MSR
                // before replacing the page and restore it only after thaw
                // has installed both the APIC mode and the guest TSC offset.
                set_msrs_state(&processor, &vp::TscDeadline::default())?;
                deadline
            } else {
                vp::TscDeadline::default()
            };
            // KVM cannot export its private timer-pending bit. For a
            // countdown one-shot, nonzero initial count with zero current
            // count can mean either pending or already delivered. Preserve
            // that image: SET_LAPIC makes it immediately due, preferring a
            // possible duplicate over losing a wakeup. For periodic timers,
            // a pending tick can coalesce and a zero count restarts a full
            // period. TSC-deadline mode is not ambiguous: KVM clears the
            // deadline MSR when transferring the expiration into the LAPIC.
            LapicTimer::disarm(&mut apic);
            write_apic(&processor, &apic)?;
            vps.push(FrozenVpTime {
                timer: Some(timer),
                deadline,
                ..Default::default()
            });
        }
        // There is no atomic clock/timer snapshot UAPI. Disarm countdowns
        // before sampling clocks, so this skew delays rather than advances
        // their expiration relative to the frozen reference time.
        let clock_ns = if matches!(self.tsc_access, TscAccess::CommonClock { .. }) {
            let sample = sample_clock(kvm)?;
            for (apic_id, frozen) in apic_ids.zip(&mut vps) {
                // This backend does not configure TSC scaling.
                frozen.tsc = sample.host_tsc.wrapping_add(kvm.vp(apic_id).tsc_offset()?);
            }
            sample.clock.clock
        } else {
            for (apic_id, frozen) in apic_ids.zip(&mut vps) {
                frozen.tsc = get_msrs_state::<vp::Tsc, 1>(&kvm.vp(apic_id))?.value;
            }
            kvm.get_clock_ns()?.clock
        };
        self.frozen = Some(FrozenTime { clock_ns, vps });
        Ok(())
    }

    fn thaw(
        &mut self,
        kvm: &kvm::Partition,
        apic_ids: impl Iterator<Item = u32> + Clone,
    ) -> Result<(), KvmError> {
        let Some(frozen) = self.frozen.as_ref() else {
            return Ok(());
        };
        // Flags are zero: REALTIME would add the paused interval back.
        kvm.set_clock_ns(frozen.clock_ns)?;
        if let TscAccess::CommonClock { khz } = self.tsc_access {
            let sample = sample_clock(kvm)?;
            let elapsed = sample.clock.clock.wrapping_sub(frozen.clock_ns);
            for (apic_id, saved) in apic_ids.clone().zip(&frozen.vps) {
                kvm.vp(apic_id).set_tsc_offset(thaw_offset(
                    saved.tsc,
                    elapsed,
                    khz,
                    sample.host_tsc,
                ))?;
            }
        } else {
            for (apic_id, saved) in apic_ids.clone().zip(&frozen.vps) {
                self.tsc_access.restore(&kvm.vp(apic_id), saved.tsc)?;
            }
        }
        for (apic_id, saved) in apic_ids.zip(&frozen.vps) {
            let processor = kvm.vp(apic_id);
            if let Some(timer) = saved.timer {
                // Keep interrupts received while stopped. Do not replace
                // IRR/ISR with the LAPIC image captured at freeze.
                let mut apic = read_apic(&processor)?;
                timer.apply(&mut apic);
                write_apic(&processor, &apic)?;
                if deadline_mode(&apic) {
                    set_msrs_state(&processor, &saved.deadline)?;
                }
            }
            if let Some(stimers) = &saved.restored_stimers {
                write_stimers(&processor, stimers)?;
            }
        }
        self.frozen = None;
        Ok(())
    }
}

impl KvmPartitionInner {
    fn freeze_time(&self) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        let routing = self.gsi_routing.lock();
        routing.with_irqfds_suspended(&self.kvm, || {
            time.freeze(&self.kvm, self.vps.iter().map(|vp| vp.vp_info.apic_id))
        })
    }

    fn thaw_time(&self) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        let routing = self.gsi_routing.lock();
        routing.with_irqfds_suspended(&self.kvm, || {
            time.thaw(&self.kvm, self.vps.iter().map(|vp| vp.vp_info.apic_id))
        })
    }

    pub(crate) fn read_reference_time(&self) -> Result<kvm::kvm_clock_data, KvmError> {
        let time = self.time.lock();
        if let Some(frozen) = &time.frozen {
            // There is no current UTC correlation for a clock that is stopped.
            return Ok(kvm::kvm_clock_data {
                clock: frozen.clock_ns,
                ..Default::default()
            });
        }
        Ok(self.kvm.get_clock_ns()?)
    }

    pub(crate) fn set_reference_time(&self, clock_ns: u64) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        if let Some(frozen) = &mut time.frozen {
            frozen.clock_ns = clock_ns;
        } else {
            self.kvm.set_clock_ns(clock_ns)?;
        }
        Ok(())
    }

    pub(crate) fn read_tsc(&self, vp: VpIndex) -> Result<vp::Tsc, KvmError> {
        let time = self.time.lock();
        if let Some(frozen) = &time.frozen {
            return Ok(vp::Tsc {
                value: frozen.vps[vp.index() as usize].tsc,
            });
        }
        get_msrs_state(&self.vp_kvm(vp))
    }

    pub(crate) fn write_tsc(&self, vp: VpIndex, value: &vp::Tsc) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        if let Some(frozen) = &mut time.frozen {
            frozen.vps[vp.index() as usize].tsc = value.value;
        } else {
            time.tsc_access.write(&self.vp_kvm(vp), value.value)?;
        }
        Ok(())
    }

    pub(crate) fn read_apic(&self, vp: VpIndex) -> Result<vp::Apic, KvmError> {
        let time = self.time.lock();
        let mut apic = read_apic(&self.vp_kvm(vp))?;
        if let Some(frozen) = &time.frozen
            && let Some(timer) = &frozen.vps[vp.index() as usize].timer
        {
            timer.apply(&mut apic);
        }
        Ok(apic)
    }

    pub(crate) fn read_tsc_deadline(&self, vp: VpIndex) -> Result<vp::TscDeadline, KvmError> {
        let time = self.time.lock();
        if let Some(frozen) = &time.frozen {
            return Ok(frozen.vps[vp.index() as usize].deadline);
        }
        get_msrs_state(&self.vp_kvm(vp))
    }

    pub(crate) fn write_tsc_deadline(
        &self,
        vp: VpIndex,
        value: &vp::TscDeadline,
    ) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        if let Some(frozen) = &mut time.frozen {
            frozen.vps[vp.index() as usize].set_deadline(*value);
        } else {
            set_msrs_state(&self.vp_kvm(vp), value)?;
        }
        Ok(())
    }

    pub(crate) fn write_apic(&self, vp: VpIndex, value: &vp::Apic) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        let routing = self.gsi_routing.lock();
        routing.with_irqfds_suspended(&self.kvm, || {
            let mut apic = value.clone();
            if time.frozen.is_some() {
                LapicTimer::disarm(&mut apic);
            }
            write_apic(&self.vp_kvm(vp), &apic)?;
            if let Some(frozen) = &mut time.frozen {
                frozen.vps[vp.index() as usize].set_apic(value);
            }
            Ok::<_, KvmError>(())
        })
    }

    pub(crate) fn restored_stimers(&self, vp: VpIndex) -> Option<vp::SynicTimers> {
        self.time
            .lock()
            .frozen
            .as_ref()
            .and_then(|frozen| frozen.vps[vp.index() as usize].restored_stimers)
    }

    pub(crate) fn write_stimers(
        &self,
        vp: VpIndex,
        value: &vp::SynicTimers,
    ) -> Result<(), KvmError> {
        let mut time = self.time.lock();
        if let Some(frozen) = &mut time.frozen {
            // Only reset/restore writes are staged. Ordinary pause leaves
            // KVM's private phase and pending state alone: on reentry KVM
            // updates the clock before processing synthetic timers, which
            // recheck their deadlines against the rebased reference time.
            // Config/count writes lose periodic phase and undelivered
            // messages; the UAPI cannot restore those across a snapshot.
            frozen.vps[vp.index() as usize].restored_stimers = Some(*value);
        } else {
            write_stimers(&self.vp_kvm(vp), value)?;
        }
        Ok(())
    }
}

fn write_stimers(kvm: &kvm::Processor<'_>, value: &vp::SynicTimers) -> Result<(), kvm::Error> {
    let mut msrs = [(0, 0); 8];
    for (i, timer) in value.timers.iter().enumerate() {
        msrs[i * 2] = (
            hvdef::HV_X64_MSR_STIMER0_CONFIG + i as u32 * 2,
            timer.config,
        );
        msrs[i * 2 + 1] = (hvdef::HV_X64_MSR_STIMER0_COUNT + i as u32 * 2, timer.count);
    }
    kvm.set_msrs(&msrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use test_with_tracing::test;

    #[test]
    fn ordinary_partitions_always_have_a_clock_strategy() {
        for common_khz in [None, Some(3_000_000)] {
            for offsets in [false, true] {
                assert_eq!(
                    TscAccess::select(true, offsets, common_khz),
                    TscAccess::Unsupported
                );
                assert_ne!(
                    TscAccess::select(false, offsets, common_khz),
                    TscAccess::Unsupported
                );
            }
        }
        assert_eq!(TscAccess::select(false, false, None), TscAccess::Msr);
        assert_eq!(TscAccess::select(false, true, None), TscAccess::Offset);
        assert_eq!(
            TscAccess::select(false, true, Some(3_000_000)),
            TscAccess::CommonClock { khz: 3_000_000 }
        );
    }

    #[test]
    fn fallback_rebase_preserves_zero_wraparound_and_vp_skew() {
        for host in [0, 123_456, u64::MAX - 10] {
            for old_offset in [0, 17, u64::MAX - 42] {
                let current = host.wrapping_add(old_offset);
                let zero = rebase_offset(old_offset, current, 0);
                assert_eq!(host.wrapping_add(zero), 0);
                for target in [1, 123_456, u64::MAX] {
                    let offset = rebase_offset(old_offset, current, target);
                    assert_eq!(host.wrapping_add(offset), target);
                    assert_eq!(offset.wrapping_sub(zero), target);
                }
            }
        }
    }

    #[test]
    fn legacy_restore_breaks_matching_without_a_zero_write() {
        for value in [0, 1, 123_456, 1 << 63, u64::MAX] {
            let [unmatched, target] = legacy_tsc_restore_values(value);
            assert_ne!(target, 0);
            assert_eq!(target.wrapping_sub(value), u64::from(value == 0));
            assert_eq!(unmatched.wrapping_sub(target), 1 << 63);
        }
    }

    #[test]
    #[ignore = "requires /dev/kvm"]
    fn kvm_fallback_excludes_short_pauses_and_restores_zero() -> Result<(), KvmError> {
        let kvm = kvm::Kvm::new()?;
        let mut vm = kvm.new_vm(kvm::VmType::Default)?;
        vm.enable_split_irqchip(24)?;
        vm.add_vp(0)?;
        vm.add_vp(1)?;
        let mut time = PartitionTime::new(&vm, 0, 2, false, true)?;
        assert!(time.is_supported());
        assert!(!matches!(time.tsc_access, TscAccess::CommonClock { .. }));
        let khz = vm.vp(0).tsc_khz()?;
        for access in [time.tsc_access, TscAccess::Msr] {
            time.tsc_access = access;
            for values in [[0, 123_456], [123_456, 0], [u64::MAX - 1_000, 1]] {
                let frozen = time.frozen.as_mut().unwrap();
                frozen.clock_ns = 3_000_000_000;
                for (vp, value) in frozen.vps.iter_mut().zip(values) {
                    vp.tsc = value;
                }
                std::thread::sleep(Duration::from_millis(50));
                let before = std::time::Instant::now();
                time.thaw(&vm, [0, 1].into_iter())?;
                for (apic_id, expected) in [0, 1].into_iter().zip(values) {
                    let actual = get_msrs_state::<vp::Tsc, 1>(&vm.vp(apic_id))?.value;
                    let limit =
                        (before.elapsed().as_nanos() + 1_000_000) * u128::from(khz) / 1_000_000;
                    assert!(u128::from(actual.wrapping_sub(expected)) <= limit);
                }
                time.freeze(&vm, [0, 1].into_iter())?;
                let values: Vec<_> = time
                    .frozen
                    .as_ref()
                    .unwrap()
                    .vps
                    .iter()
                    .map(|vp| vp.tsc)
                    .collect();
                std::thread::sleep(Duration::from_millis(20));
                time.freeze(&vm, [0, 1].into_iter())?;
                assert_eq!(
                    time.frozen
                        .as_ref()
                        .unwrap()
                        .vps
                        .iter()
                        .map(|vp| vp.tsc)
                        .collect::<Vec<_>>(),
                    values
                );
            }
        }
        Ok(())
    }

    #[test]
    fn tsc_rebase_preserves_zero_and_per_vp_offsets() {
        let host = 0xffff_ffff_ffff_f000;
        let elapsed = 17_000;
        let khz = 3_000_000;
        let zero = thaw_offset(0, elapsed, khz, host);
        assert_eq!(host.wrapping_add(zero), 51_000);
        let other = thaw_offset(u64::MAX - 13, elapsed, khz, host);
        assert_eq!(other.wrapping_sub(zero), u64::MAX - 13);
        assert_eq!(thaw_offset(0, 0, khz, host).wrapping_add(host), 0);
    }

    #[test]
    fn tsc_conversion_does_not_overflow_intermediate_product() {
        let ns = 10_000_000_000_000;
        assert_eq!(thaw_offset(3, ns, 4_000_000, 5), 39_999_999_999_998);
    }

    fn timer_apic() -> vp::Apic {
        let regs = vp::ApicRegisters {
            svr: 0x1ff,
            lvt_timer: 0x40,
            timer_icr: 1_000_000_000,
            timer_ccr: 234_567,
            timer_dcr: 3,
            ..FromZeros::new_zeroed()
        };
        vp::Apic::new(
            x86defs::apic::ApicBase::new()
                .with_base_page(x86defs::apic::APIC_BASE_PAGE)
                .with_enable(true)
                .with_bsp(true),
            regs,
            [0; 8],
        )
    }

    #[test]
    fn timer_overlay_preserves_interrupts_received_while_frozen() {
        let mut apic = timer_apic();
        let timer = LapicTimer::capture(&apic);
        LapicTimer::disarm(&mut apic);
        assert_eq!(apic.registers().timer_icr, 0);
        assert_eq!(apic.registers().timer_ccr, 0);

        let mut live = *apic.registers();
        live.irr[3] = 1 << 7;
        live.isr[4] = 1 << 9;
        live.lvt_lint0 = 0x1234;
        apic.registers = *live.as_array();
        timer.apply(&mut apic);
        assert_eq!(LapicTimer::capture(&apic), timer);
        assert_eq!(apic.registers().irr, live.irr);
        assert_eq!(apic.registers().isr, live.isr);
        assert_eq!(apic.registers().lvt_lint0, live.lvt_lint0);
    }

    #[test]
    fn pending_countdown_survives_timer_staging() {
        let mut apic = timer_apic();
        let mut regs = *apic.registers();
        regs.timer_ccr = 0;
        apic.registers = *regs.as_array();
        let pending = LapicTimer::capture(&apic);
        LapicTimer::disarm(&mut apic);
        pending.apply(&mut apic);
        assert_eq!(apic.registers().timer_icr, 1_000_000_000);
        assert_eq!(apic.registers().timer_ccr, 0);
    }

    #[test]
    fn frozen_deadline_is_independent_of_apic_state_and_obeys_mode_changes() {
        let mut apic = timer_apic();
        let deadline = vp::TscDeadline {
            value: 0x1234_5678_9abc,
        };
        let mut frozen = FrozenVpTime::default();
        frozen.set_apic(&apic);
        frozen.set_deadline(deadline);
        assert_eq!(frozen.deadline.value, 0);

        let mut regs = *apic.registers();
        regs.lvt_timer = Lvt::from(regs.lvt_timer)
            .with_timer_mode(TimerMode::TSC_DEADLINE.0)
            .into();
        apic.registers = *regs.as_array();
        frozen.set_apic(&apic);
        frozen.set_deadline(deadline);
        assert_eq!(frozen.deadline, deadline);

        // An interrupt-state update in the same mode must not erase the MSR.
        regs.irr[3] = 1 << 7;
        apic.registers = *regs.as_array();
        frozen.set_apic(&apic);
        assert_eq!(frozen.deadline, deadline);

        frozen.set_deadline(vp::TscDeadline::default());
        assert_eq!(frozen.deadline.value, 0);
        frozen.set_deadline(deadline);
        frozen.set_apic(&timer_apic());
        assert_eq!(frozen.deadline.value, 0);
    }

    #[test]
    #[ignore = "requires /dev/kvm with a stable TSC and KVM_VCPU_TSC_OFFSET"]
    fn kvm_freezes_and_restores_separate_tsc_deadline() -> Result<(), KvmError> {
        let kvm = kvm::Kvm::new()?;
        let mut vm = kvm.new_vm(kvm::VmType::Default)?;
        vm.enable_split_irqchip(24)?;
        vm.add_vp(0)?;
        let mut time = PartitionTime::new(&vm, 0, 1, false, false)?;
        assert!(time.is_supported());
        let mut apic = timer_apic();
        let mut regs = *apic.registers();
        regs.lvt_timer = Lvt::from(regs.lvt_timer)
            .with_timer_mode(TimerMode::TSC_DEADLINE.0)
            .into();
        apic.registers = *regs.as_array();
        write_apic(&vm.vp(0), &apic)?;
        let deadline = vp::TscDeadline {
            value: u64::from(vm.vp(0).tsc_khz()?) * 60_000,
        };
        let saved = &mut time.frozen.as_mut().unwrap().vps[0];
        saved.set_apic(&apic);
        saved.set_deadline(deadline);
        assert_eq!(get_msrs_state::<vp::TscDeadline, 1>(&vm.vp(0))?.value, 0);
        time.thaw(&vm, [0].into_iter())?;
        assert_eq!(get_msrs_state::<vp::TscDeadline, 1>(&vm.vp(0))?, deadline);
        time.freeze(&vm, [0].into_iter())?;
        assert_eq!(time.frozen.as_ref().unwrap().vps[0].deadline, deadline);
        assert_eq!(get_msrs_state::<vp::TscDeadline, 1>(&vm.vp(0))?.value, 0);
        time.thaw(&vm, [0].into_iter())?;
        assert_eq!(get_msrs_state::<vp::TscDeadline, 1>(&vm.vp(0))?, deadline);
        Ok(())
    }

    #[test]
    #[ignore = "requires /dev/kvm with a stable TSC and KVM_VCPU_TSC_OFFSET"]
    fn kvm_freezes_clocks_and_preserves_remaining_countdown() -> Result<(), KvmError> {
        let kvm = kvm::Kvm::new()?;
        let mut vm = kvm.new_vm(kvm::VmType::Default)?;
        vm.enable_split_irqchip(24)?;
        vm.add_vp(0)?;
        vm.add_vp(1)?;
        let mut time = PartitionTime::new(&vm, 0, 2, false, false)?;
        assert!(time.is_supported());
        let TscAccess::CommonClock { khz } = time.tsc_access else {
            panic!("test requires common-clock support");
        };
        let frozen = time.frozen.as_mut().unwrap();
        frozen.clock_ns = 3_000_000_000;
        frozen.vps[1].tsc = 123_456;
        let mut apic = timer_apic();
        let mut regs = *apic.registers();
        regs.timer_ccr = regs.timer_icr;
        apic.registers = *regs.as_array();
        frozen.vps[0].timer = Some(LapicTimer::capture(&apic));
        LapicTimer::disarm(&mut apic);
        write_apic(&vm.vp(0), &apic)?;

        time.thaw(&vm, [0, 1].into_iter())?;
        let mut tsc = [0];
        vm.vp(0).get_msrs(&[x86defs::X86X_MSR_TSC], &mut tsc)?;
        assert!(tsc[0] < u64::from(khz) * 1_000);
        assert_eq!(
            vm.vp(1).tsc_offset()?.wrapping_sub(vm.vp(0).tsc_offset()?),
            123_456
        );
        assert!(vm.get_clock_ns()?.clock >= 3_000_000_000);
        let offset = vm.vp(0).tsc_offset()?;
        time.thaw(&vm, [0, 1].into_iter())?;
        assert_eq!(vm.vp(0).tsc_offset()?, offset);

        std::thread::sleep(Duration::from_millis(20));
        time.freeze(&vm, [0, 1].into_iter())?;
        let frozen = time.frozen.as_ref().unwrap();
        let clock = frozen.clock_ns;
        let saved_tsc = frozen.vps[0].tsc;
        let timer = frozen.vps[0].timer.unwrap();
        assert!(timer.remaining > 0 && timer.remaining < timer.initial);
        assert_eq!(frozen.vps[1].tsc.wrapping_sub(frozen.vps[0].tsc), 123_456);
        assert_eq!(read_apic(&vm.vp(0))?.registers().timer_icr, 0);

        std::thread::sleep(Duration::from_millis(20));
        time.freeze(&vm, [0, 1].into_iter())?;
        assert_eq!(time.frozen.as_ref().unwrap().clock_ns, clock);
        let mut live = read_apic(&vm.vp(0))?;
        let mut regs = *live.registers();
        regs.irr[3] |= 1 << 7;
        live.registers = *regs.as_array();
        write_apic(&vm.vp(0), &live)?;
        let before_thaw = std::time::Instant::now();
        time.thaw(&vm, [0, 1].into_iter())?;
        let resumed_clock = vm.get_clock_ns()?.clock;
        vm.vp(0).get_msrs(&[x86defs::X86X_MSR_TSC], &mut tsc)?;
        let max_elapsed_ns = before_thaw.elapsed().as_nanos() + 1_000_000;
        assert!(resumed_clock >= clock);
        assert!(u128::from(resumed_clock - clock) <= max_elapsed_ns);
        assert!(
            u128::from(tsc[0].wrapping_sub(saved_tsc))
                <= max_elapsed_ns * u128::from(khz) / 1_000_000
        );
        let resumed = read_apic(&vm.vp(0))?;
        assert_ne!(resumed.registers().irr[3] & (1 << 7), 0);
        assert!(resumed.registers().timer_ccr > 0);
        assert!(resumed.registers().timer_ccr <= timer.remaining);
        assert_eq!(resumed.registers().timer_icr, timer.initial);
        Ok(())
    }
}
