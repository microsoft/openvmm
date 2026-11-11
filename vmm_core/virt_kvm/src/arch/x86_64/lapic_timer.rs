// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The expired one-shot LAPIC timer of a stopped VP.
//!
//! KVM reports a one-shot timer that expired, and whose interrupt the guest
//! took, with the same LAPIC state as one whose expiry KVM recorded but has
//! not injected yet: an initial count, a current count of 0, and the vector
//! clear in IRR. `KVM_SET_LAPIC` fires such a timer, at once since Linux 6.3
//! and after a full period from 5.8 to 6.2, which the second state needs, so
//! a restore of the first gives the guest a timer interrupt that it never
//! programmed.
//!
//! So when a VP stops, the backend reads its timer's count, and then has KVM
//! deliver the expiries that it recorded
//! ([`kvm::VpRunner::deliver_pending_interrupts`]). From then on, KVM holds
//! only an expiry after that read, and the LAPIC state that the backend
//! reports for the stopped VP tells the two states apart: an expired timer
//! whose vector is clear in IRR expired after the stop if it was counting
//! then, and KVM holds its interrupt; otherwise the guest took it. The state
//! queues the interrupt in IRR in the first case, which `set_apic` restores
//! without starting the timer again, and has an initial count of 0 in the
//! second, which `KVM_SET_LAPIC` does not start.
//!
//! KVM records an expiry when the host timer behind the LAPIC timer fires,
//! which can be a moment after the count reads 0, and longer on a loaded
//! host. So if the timer had expired at the stop, and its vector is still
//! clear in IRR after the delivery, the VP has KVM deliver again, at growing
//! intervals, until the vector is set or [`EXPIRY_RECORD_TIME`] after the
//! read, before the state reports the interrupt as taken. The VP measures
//! that time with the host's monotonic clock, or with the pauses between the
//! deliveries if the clock steps back, so that such a step cannot stretch the
//! wait. If KVM records the expiry even later, a restore does not deliver its
//! interrupt.
//!
//! KVM records no expiry of the VMX preemption timer, which it can use for the
//! timer of a running VP, while the VP is outside guest mode. On such a host,
//! a timer that expired between the VP's last exit from guest mode and the
//! read may still be held, so the state keeps an expired timer that was not
//! counting at the stop as KVM reports it, and a restore fires it again.

use std::time::Duration;
use std::time::Instant;
use virt::x86::vp::ApicRegisters;
use x86defs::apic::ApicBase;
use x86defs::apic::DeliveryMode;
use x86defs::apic::Lvt;
use x86defs::apic::Svr;
use x86defs::apic::TimerMode;
use x86defs::cpuid::Vendor;

/// How long after a LAPIC timer's count reads 0 KVM may take to record its
/// expiry, when the host signals the host timer late. On an Azure VM with
/// nested KVM, with every CPU busy, 59,000 tries recorded it within 4 ms.
const EXPIRY_RECORD_TIME: Duration = Duration::from_millis(20);

/// The shortest and the longest pause before KVM delivers again while it may
/// not have recorded an expiry yet.
const MIN_PAUSE: Duration = Duration::from_micros(50);
const MAX_PAUSE: Duration = Duration::from_millis(1);

/// A VP's LAPIC timer when the VP stopped.
#[derive(Debug, Copy, Clone)]
pub(crate) struct TimerAtStop {
    /// The current count, read before KVM delivered the expiries that it had
    /// recorded.
    current_count: u32,
    /// When the current count was read.
    read_at: Instant,
    /// The pauses that the VP slept since then before KVM delivered again.
    slept: Duration,
    /// Whether KVM last delivered once it must have recorded the timer's
    /// expiry.
    delivered_late: bool,
    /// Whether KVM had no expiry of the timer to deliver even then, so the
    /// guest took the timer's interrupt.
    interrupt_taken: bool,
}

impl TimerAtStop {
    /// Reads the timer of the stopped VP `vp`, or returns `None` if KVM does
    /// not report its LAPIC state.
    pub fn read(vp: &kvm::Processor<'_>) -> Option<Self> {
        let (_, registers) = read_apic(vp)?;
        Some(Self {
            current_count: registers.timer_ccr,
            read_at: Instant::now(),
            slept: Duration::ZERO,
            delivered_late: false,
            interrupt_taken: false,
        })
    }

    /// After KVM delivered the expiries that it had recorded, returns whether
    /// it must deliver again because KVM may not have recorded the expiry of
    /// a timer that had expired at the stop yet, its vector being clear in
    /// IRR, after pausing for longer each time. `preemption_timer` is whether
    /// KVM may run the timer on the VMX preemption timer, which makes waiting
    /// useless.
    pub fn await_expiry(&mut self, vp: &kvm::Processor<'_>, preemption_timer: bool) -> bool {
        if self.interrupt_taken || self.current_count != 0 || preemption_timer {
            return false;
        }
        let Some((apic_base, registers)) = read_apic(vp) else {
            return false;
        };
        if !expired_timer(apic_base, &registers)
            .is_some_and(|(bank, bit)| registers.irr[bank] & bit == 0)
        {
            return false;
        }
        let read_at = self.read_at;
        self.pause_for_expiry(|| read_at.elapsed(), std::thread::sleep)
    }

    /// Pauses with `sleep` and returns `true`, for KVM to deliver again, while
    /// KVM may not have recorded the timer's expiry yet; once KVM delivered
    /// [`EXPIRY_RECORD_TIME`] after the read, returns `false` instead, the
    /// guest having taken the timer's interrupt. `elapsed` returns the time
    /// since the read that `Instant` measures.
    ///
    /// The VP has waited for the longer of that time and the pauses that it
    /// slept, and pauses for longer the longer it has waited. With a monotonic
    /// clock, the time is the longer, since a pause never ends early and the
    /// time also counts the deliveries, so the clock ends the wait. If the
    /// clock steps back, `Instant` measures less, or nothing, until the clock
    /// is back where it was, and the pauses end the wait instead, still
    /// [`EXPIRY_RECORD_TIME`] after the read.
    fn pause_for_expiry(
        &mut self,
        elapsed: impl Fn() -> Duration,
        sleep: impl FnOnce(Duration),
    ) -> bool {
        if self.delivered_late {
            self.interrupt_taken = true;
            return false;
        }
        let waited = elapsed().max(self.slept);
        let pause = waited
            .clamp(MIN_PAUSE, MAX_PAUSE)
            .min(EXPIRY_RECORD_TIME.saturating_sub(waited));
        sleep(pause);
        self.slept += pause;
        self.delivered_late = elapsed().max(self.slept) >= EXPIRY_RECORD_TIME;
        true
    }

    /// Makes `registers`, the LAPIC state that KVM reports for the VP while
    /// it is still stopped, report an expired one-shot timer exactly.
    pub fn settle(self, apic_base: u64, registers: &mut ApicRegisters) {
        let Some((bank, bit)) = expired_timer(apic_base, registers) else {
            return;
        };
        if registers.irr[bank] & bit != 0 {
            return;
        }
        if self.current_count != 0 {
            // The timer expired after the stop, so KVM holds its interrupt.
            registers.irr[bank] |= bit;
            registers.tmr[bank] &= !bit;
        } else if self.interrupt_taken {
            // The timer expired before the stop, and KVM then delivered any
            // interrupt that it held, so the guest has taken this one.
            registers.timer_icr = 0;
        }
    }
}

/// Reads the APIC base and the LAPIC state of the stopped VP `vp`.
fn read_apic(vp: &kvm::Processor<'_>) -> Option<(u64, ApicRegisters)> {
    let mut apic_base = 0;
    vp.get_msrs(
        &[x86defs::X86X_MSR_APIC_BASE],
        std::slice::from_mut(&mut apic_base),
    )
    .ok()?;
    let mut page = [0; 1024];
    vp.get_lapic(&mut page).ok()?;
    Some((apic_base, ApicRegisters::from_page(&page)))
}

/// Returns the IRR bank and bit of the vector of an expired one-shot timer
/// that interrupts the VP through an enabled APIC, or `None` for any other
/// timer.
fn expired_timer(apic_base: u64, registers: &ApicRegisters) -> Option<(usize, u32)> {
    let lvt = Lvt::from(registers.lvt_timer);
    let vector = lvt.vector();
    (ApicBase::from(apic_base).enable()
        && Svr::from(registers.svr).enable()
        && TimerMode(lvt.timer_mode()) == TimerMode::ONE_SHOT
        && !lvt.masked()
        && DeliveryMode(lvt.delivery_mode()) == DeliveryMode::FIXED
        && vector >= 16
        && registers.timer_icr != 0
        && registers.timer_ccr == 0)
        .then(|| (usize::from(vector / 32), 1 << (vector % 32)))
}

/// Returns whether KVM may run LAPIC timers on the VMX preemption timer.
pub(crate) fn kvm_may_use_preemption_timer() -> bool {
    // Only kvm_intel has a hypervisor timer.
    let leaf = safe_intrinsics::cpuid(0, 0);
    if !Vendor::from_ebx_ecx_edx(leaf.ebx, leaf.ecx, leaf.edx).is_intel_compatible() {
        return false;
    }
    // Assume that it may when the parameter cannot be read.
    std::fs::read_to_string("/sys/module/kvm_intel/parameters/preemption_timer")
        .map_or(true, |value| value.trim() != "N")
}

#[cfg(test)]
mod tests {
    use super::EXPIRY_RECORD_TIME;
    use super::TimerAtStop;
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use virt::x86::vp::ApicRegisters;
    use zerocopy::FromZeros;

    const VECTOR: u32 = 0x40;
    const BANK: usize = VECTOR as usize / 32;
    const BIT: u32 = 1 << (VECTOR % 32);
    /// An enabled xAPIC.
    const APIC_BASE: u64 = 0xfee0_0000 | (1 << 11) | (1 << 8);
    const MASKED: u32 = 1 << 16;

    fn expired_timer(lvt_timer: u32) -> ApicRegisters {
        ApicRegisters {
            svr: 0x1ff,
            lvt_timer,
            timer_icr: 1000,
            timer_ccr: 0,
            timer_dcr: 0xb,
            ..FromZeros::new_zeroed()
        }
    }

    fn settle(apic_base: u64, registers: &mut ApicRegisters, count: u32, interrupt_taken: bool) {
        TimerAtStop {
            current_count: count,
            read_at: Instant::now(),
            slept: Duration::ZERO,
            delivered_late: interrupt_taken,
            interrupt_taken,
        }
        .settle(apic_base, registers);
    }

    /// How a VP waited for KVM to record the expiry of a timer that had
    /// expired when it stopped.
    struct ExpiryWait {
        /// The pauses before the deliveries, in order.
        pauses: Vec<Duration>,
        /// How long after the read KVM last delivered.
        last_delivery: Duration,
    }

    impl ExpiryWait {
        fn slept(&self) -> Duration {
            self.pauses.iter().sum()
        }
    }

    /// Waits for an expiry that KVM never records, with deliveries that take
    /// `delivery` each, while `Instant` measures `clock(time)` `time` after
    /// the read.
    fn wait_for_a_lost_expiry(
        clock: impl Fn(Duration) -> Duration,
        delivery: Duration,
    ) -> ExpiryWait {
        let mut timer = TimerAtStop {
            current_count: 0,
            read_at: Instant::now(),
            slept: Duration::ZERO,
            delivered_late: false,
            interrupt_taken: false,
        };
        // KVM delivered once before the first pause.
        let time = Cell::new(delivery);
        let pauses = RefCell::new(Vec::new());
        let mut last_delivery = Duration::ZERO;
        while timer.pause_for_expiry(
            || clock(time.get()),
            |pause| {
                pauses.borrow_mut().push(pause);
                time.set(time.get() + pause);
            },
        ) {
            last_delivery = time.get();
            time.set(time.get() + delivery);
            assert!(pauses.borrow().len() < 1_000, "the wait does not end");
        }
        assert!(timer.interrupt_taken);
        ExpiryWait {
            pauses: pauses.into_inner(),
            last_delivery,
        }
    }

    #[test]
    fn a_monotonic_clock_ends_the_wait_for_an_expiry() {
        // The clock also counts the deliveries, so it reaches the end of the
        // wait before the pauses do.
        let wait = wait_for_a_lost_expiry(|time| time, Duration::from_micros(100));
        assert!(wait.last_delivery >= EXPIRY_RECORD_TIME);
        assert!(wait.slept() < EXPIRY_RECORD_TIME);
    }

    #[test]
    fn a_clock_that_steps_back_does_not_stretch_the_wait_for_an_expiry() {
        const DELIVERY: Duration = Duration::from_micros(20);
        // Once the clock steps back past the read, `Instant` measures nothing
        // since the read until the clock is back where it was. The pauses
        // grow as if the clock measured only them, and end the wait, still
        // EXPIRY_RECORD_TIME after the read.
        let behind = wait_for_a_lost_expiry(|_| Duration::ZERO, DELIVERY);
        assert_eq!(
            behind.pauses,
            wait_for_a_lost_expiry(|time| time, Duration::ZERO).pauses
        );
        assert_eq!(behind.slept(), EXPIRY_RECORD_TIME);
        assert!(behind.last_delivery >= EXPIRY_RECORD_TIME);

        // The clock steps back by 10 ms 5 ms after the read, so it measures
        // EXPIRY_RECORD_TIME some 10 ms after the pauses add up to it.
        let stepped = wait_for_a_lost_expiry(
            |time| {
                if time < Duration::from_millis(5) {
                    time
                } else {
                    time.saturating_sub(Duration::from_millis(10))
                }
            },
            DELIVERY,
        );
        assert_eq!(stepped.slept(), EXPIRY_RECORD_TIME);
        assert!(stepped.last_delivery >= EXPIRY_RECORD_TIME);
    }

    #[test]
    fn a_timer_that_expired_after_the_stop_is_pending() {
        for interrupt_taken in [false, true] {
            let mut registers = expired_timer(VECTOR);
            // An earlier level-triggered interrupt of the vector set its bit.
            registers.tmr[BANK] = BIT;
            settle(APIC_BASE, &mut registers, 1, interrupt_taken);
            assert_eq!(registers.irr[BANK], BIT);
            assert_eq!(registers.tmr[BANK], 0);
            assert_eq!(registers.timer_icr, 1000);
        }
    }

    #[test]
    fn a_timer_that_expired_before_the_stop_is_idle() {
        let mut registers = expired_timer(VECTOR);
        settle(APIC_BASE, &mut registers, 0, true);
        assert_eq!(registers.irr[BANK], 0);
        assert_eq!(registers.timer_icr, 0);

        // Until KVM must have recorded the expiry, or with the VMX preemption
        // timer, KVM may still hold its interrupt.
        let mut registers = expired_timer(VECTOR);
        settle(APIC_BASE, &mut registers, 0, false);
        assert_eq!(registers.irr[BANK], 0);
        assert_eq!(registers.timer_icr, 1000);
    }

    #[test]
    fn other_timers_are_left_as_kvm_reports_them() {
        let mut queued = expired_timer(VECTOR);
        queued.irr[BANK] = BIT;
        let cases = [
            (
                APIC_BASE,
                ApicRegisters {
                    timer_ccr: 1,
                    ..expired_timer(VECTOR)
                },
            ),
            (
                APIC_BASE,
                ApicRegisters {
                    timer_icr: 0,
                    ..expired_timer(VECTOR)
                },
            ),
            (APIC_BASE, queued),
            (APIC_BASE, expired_timer(VECTOR | MASKED)),
            // Periodic and TSC-deadline modes.
            (APIC_BASE, expired_timer(VECTOR | 1 << 17)),
            (APIC_BASE, expired_timer(VECTOR | 2 << 17)),
            // The NMI delivery mode, and an illegal vector.
            (APIC_BASE, expired_timer(VECTOR | 4 << 8)),
            (APIC_BASE, expired_timer(15)),
            // A software-disabled and a hardware-disabled APIC.
            (
                APIC_BASE,
                ApicRegisters {
                    svr: 0xff,
                    ..expired_timer(VECTOR)
                },
            ),
            (APIC_BASE & !(1 << 11), expired_timer(VECTOR)),
        ];
        for (apic_base, registers) in cases {
            for count in [0, 1] {
                for interrupt_taken in [false, true] {
                    let mut settled = registers;
                    settle(apic_base, &mut settled, count, interrupt_taken);
                    assert_eq!(
                        settled.as_array(),
                        registers.as_array(),
                        "{registers:x?}, count {count}, interrupt taken {interrupt_taken}"
                    );
                }
            }
        }
    }
}

/// Tests against the host's KVM. Each runs a real-mode guest in x2APIC mode,
/// whose LAPIC timer handler EOIs the interrupt and writes `TIMER_PORT`, and
/// stops, saves, and restores the VP as the backend does.
#[cfg(test)]
mod kvm_tests {
    use super::super::test_guest::GuestRam;
    use super::super::vp_state::kvm_lapic_page;
    use super::TimerAtStop;
    use super::kvm_may_use_preemption_timer;
    use kvm::Exit;
    use std::time::Duration;
    use std::time::Instant;
    use test_with_tracing::test;
    use virt::x86::vp::ApicRegisters;

    const VECTOR: u8 = 0x40;
    const BANK: usize = VECTOR as usize / 32;
    const BIT: u32 = 1 << (VECTOR % 32);
    const CODE_GPA: u64 = 0x1000;
    const HANDLER_GPA: u64 = 0x2000;
    const COUNTER_GPA: u64 = 0x3000;
    const WOKE_PORT: u8 = 0x80;
    const TIMER_PORT: u8 = 0x81;
    /// An enabled x2APIC.
    const APIC_BASE: u64 = 0xfee0_0000 | (1 << 11) | (1 << 10) | (1 << 8);

    /// How the guest waits for its timer.
    #[derive(Copy, Clone, Debug)]
    enum Idle {
        /// `sti; hlt; out WOKE_PORT, al` in a loop: the VP stops halted.
        Halt,
        /// `sti`, then incrementing a counter in a loop: the VP stops
        /// runnable, with interrupts enabled.
        Spin,
    }

    /// A VP, with a timer that forces it out of `KVM_RUN` at a deadline.
    struct Guest<'a> {
        vp: kvm::Processor<'a>,
        runner: kvm::VpRunner<'a>,
        ram: &'a GuestRam<'a>,
        stop_timer: StopTimer,
    }

    impl Guest<'_> {
        /// Arms a one-shot timer of `ns` at KVM's 1-GHz APIC bus, divided by
        /// 1.
        fn arm_timer(&self, ns: u32) {
            let mut page = [0; 1024];
            self.vp.get_lapic(&mut page).unwrap();
            let mut registers = ApicRegisters::from_page(&page);
            registers.svr = 0x1ff;
            registers.lvt_timer = VECTOR.into();
            registers.timer_dcr = 0xb;
            registers.timer_icr = ns;
            // KVM starts the timer from the current count.
            registers.timer_ccr = ns;
            self.vp.set_lapic(&registers.as_page()).unwrap();
        }

        /// Runs the VP for `duration`, and returns the ports that it wrote.
        /// The VP is then outside `KVM_RUN`, as when it stops, possibly with
        /// a pending exit.
        fn run_for(&mut self, duration: Duration) -> Vec<u8> {
            let deadline = Instant::now() + duration;
            self.stop_timer.set(duration);
            let mut ports = Vec::new();
            while Instant::now() < deadline {
                match self.runner.run().unwrap() {
                    Exit::Interrupted => {}
                    Exit::IoOut { port, .. } => ports.push(port as u8),
                    exit => panic!("unexpected exit: {exit:?}"),
                }
            }
            self.stop_timer.set(Duration::ZERO);
            ports
        }

        fn timer_interrupts(&mut self, duration: Duration) -> usize {
            self.run_for(duration)
                .into_iter()
                .filter(|&port| port == TIMER_PORT)
                .count()
        }

        /// Stops the VP as `run_vp` does: reads the timer, and then has KVM
        /// deliver the interrupts that it holds, again once it must have
        /// recorded an expiry.
        fn stop(&mut self) -> Option<TimerAtStop> {
            let mut timer = TimerAtStop::read(&self.vp);
            loop {
                match self.runner.deliver_pending_interrupts().unwrap() {
                    Exit::Interrupted => {}
                    exit => panic!("unexpected exit: {exit:?}"),
                }
                if !timer.as_mut().is_some_and(|timer| {
                    timer.await_expiry(&self.vp, kvm_may_use_preemption_timer())
                }) {
                    break timer;
                }
            }
        }

        /// Returns the LAPIC state of the stopped VP, as `apic` does.
        fn save(&self, timer: Option<TimerAtStop>) -> ApicRegisters {
            let mut apic_base = 0;
            self.vp
                .get_msrs(
                    &[x86defs::X86X_MSR_APIC_BASE],
                    std::slice::from_mut(&mut apic_base),
                )
                .unwrap();
            let mut page = [0; 1024];
            self.vp.get_lapic(&mut page).unwrap();
            let mut registers = ApicRegisters::from_page(&page);
            if let Some(timer) = timer {
                timer.settle(apic_base, &mut registers);
            }
            registers
        }

        /// Sets the LAPIC state, as `set_apic` does.
        fn restore(&self, registers: &ApicRegisters) {
            self.vp
                .set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, APIC_BASE)])
                .unwrap();
            self.vp.set_lapic(&kvm_lapic_page(registers)).unwrap();
        }

        fn rip_and_counter(&self) -> (u64, u64) {
            (
                self.vp.get_regs().unwrap().rip,
                self.ram.read_u64(COUNTER_GPA),
            )
        }
    }

    /// A timer that sends the VP cancellation signal to this thread, as
    /// `Processor::force_exit` does, to force the VP out of `KVM_RUN` at a
    /// deadline.
    struct StopTimer(libc::timer_t);

    impl StopTimer {
        fn new() -> Self {
            // SAFETY: all-zero values are valid for the calls to fill, and
            // the calls are as documented.
            unsafe {
                let mut event: libc::sigevent = std::mem::zeroed();
                event.sigev_notify = libc::SIGEV_THREAD_ID;
                event.sigev_signo = libc::SIGRTMIN();
                event.sigev_notify_thread_id = libc::gettid();
                let mut timer = std::mem::zeroed();
                assert_eq!(
                    libc::timer_create(libc::CLOCK_MONOTONIC, &mut event, &mut timer),
                    0
                );
                Self(timer)
            }
        }

        /// Sends the signal after `duration`, or never if it is zero.
        fn set(&self, duration: Duration) {
            let value = libc::itimerspec {
                it_interval: libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                },
                it_value: libc::timespec {
                    tv_sec: duration.as_secs() as _,
                    tv_nsec: duration.subsec_nanos().into(),
                },
            };
            // SAFETY: calling as documented, with a timer that `new` created.
            let r = unsafe { libc::timer_settime(self.0, 0, &value, std::ptr::null_mut()) };
            assert_eq!(r, 0);
        }
    }

    impl Drop for StopTimer {
        fn drop(&mut self) {
            // SAFETY: deleting the timer that `new` created.
            unsafe { libc::timer_delete(self.0) };
        }
    }

    fn with_guest(idle: Idle, f: impl FnOnce(&mut Guest<'_>)) {
        kvm::init();
        let kvm = kvm::Kvm::new().unwrap();
        let mut partition = kvm.new_vm(kvm::VmType::Default).unwrap();
        partition.enable_split_irqchip(24).unwrap();
        partition.enable_x2apic_api().unwrap();
        partition.add_vp(0).unwrap();

        let ram = GuestRam::new(&partition, 0x10000);
        // The real-mode IVT entry for the vector.
        ram.write(
            u64::from(VECTOR) * 4,
            &[HANDLER_GPA as u8, (HANDLER_GPA >> 8) as u8, 0, 0],
        );
        match idle {
            // sti; hlt; out WOKE_PORT, al; jmp CODE_GPA
            Idle::Halt => ram.write(CODE_GPA, &[0xfb, 0xf4, 0xe6, WOKE_PORT, 0xeb, 0xfa]),
            // sti; inc word [COUNTER_GPA]; jmp CODE_GPA + 1
            Idle::Spin => ram.write(
                CODE_GPA,
                &[
                    0xfb,
                    0xff,
                    0x06,
                    COUNTER_GPA as u8,
                    (COUNTER_GPA >> 8) as u8,
                    0xeb,
                    0xfa,
                ],
            ),
        }
        // xor eax, eax; xor edx, edx; mov ecx, 0x80b (the x2APIC EOI
        // register); wrmsr; out TIMER_PORT, al; iret
        ram.write(
            HANDLER_GPA,
            &[
                0x66, 0x31, 0xc0, 0x66, 0x31, 0xd2, 0x66, 0xb9, 0x0b, 0x08, 0x00, 0x00, 0x0f, 0x30,
                0xe6, TIMER_PORT, 0xcf,
            ],
        );

        let vp = partition.vp(0);
        // Enumerate the x2APIC.
        vp.set_cpuid(&[kvm::kvm_cpuid_entry2 {
            function: 1,
            ecx: 1 << 21,
            ..Default::default()
        }])
        .unwrap();
        let mut sregs = vp.get_sregs().unwrap();
        sregs.cs.selector = 0;
        sregs.cs.base = 0;
        vp.set_sregs(&sregs).unwrap();
        vp.set_regs(&kvm::kvm_regs {
            rip: CODE_GPA,
            rsp: 0x8000,
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        vp.set_msrs(&[(x86defs::X86X_MSR_APIC_BASE, APIC_BASE)])
            .unwrap();

        f(&mut Guest {
            runner: vp.runner(),
            vp,
            ram: &ram,
            stop_timer: StopTimer::new(),
        });
    }

    /// Returns whether this thread blocks the VP cancellation signal.
    fn cancellation_blocked() -> bool {
        // SAFETY: an all-zero sigset_t is a valid value for the call to fill,
        // and the call is as documented.
        unsafe {
            let mut mask = std::mem::zeroed();
            assert_eq!(
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask),
                0
            );
            libc::sigismember(&mask, libc::SIGRTMIN()) == 1
        }
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn stopping_runs_no_guest_instruction() {
        with_guest(Idle::Spin, |guest| {
            guest.arm_timer(30_000_000);
            assert!(guest.run_for(Duration::from_millis(5)).is_empty());
            let running = guest.rip_and_counter();
            std::thread::sleep(Duration::from_millis(60));
            guest.stop();
            assert_eq!(guest.rip_and_counter(), running);
            assert!(!cancellation_blocked());
            // The VP runs on.
            assert_eq!(guest.timer_interrupts(Duration::from_millis(20)), 1);
            assert_ne!(guest.rip_and_counter(), running);
        });
    }

    /// Takes the timer's interrupt, saves and restores the VP, and returns
    /// how many timer interrupts the guest takes after that.
    fn taken_then_restored(idle: Idle) -> usize {
        let mut after = 0;
        with_guest(idle, |guest| {
            guest.arm_timer(1_000_000);
            assert_eq!(guest.timer_interrupts(Duration::from_millis(20)), 1);
            let timer = guest.stop();
            let saved = guest.save(timer);
            assert_eq!(saved.irr[BANK], 0);
            guest.restore(&saved);
            after = guest.timer_interrupts(Duration::from_millis(200));
        });
        after
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn a_taken_timer_interrupt_is_not_taken_again_after_a_restore() {
        // KVM may hold the interrupt of a VMX preemption timer, so it fires
        // the timer again.
        let expected = usize::from(kvm_may_use_preemption_timer());
        for idle in [Idle::Halt, Idle::Spin] {
            assert_eq!(taken_then_restored(idle), expected, "{idle:?}");
        }
    }

    /// Stops the VP before its timer expires, or after it expired while the
    /// VP was outside `KVM_RUN`, then saves and restores the VP, and returns
    /// how many timer interrupts the guest takes after that.
    fn expired_while_stopped_then_restored(idle: Idle, expires_before_stop: bool) -> usize {
        let mut after = 0;
        with_guest(idle, |guest| {
            guest.arm_timer(30_000_000);
            assert!(guest.run_for(Duration::from_millis(5)).is_empty());
            if expires_before_stop {
                std::thread::sleep(Duration::from_millis(60));
            }
            let timer = guest.stop();
            if !expires_before_stop {
                std::thread::sleep(Duration::from_millis(60));
            }
            let saved = guest.save(timer);
            // KVM records no expiry of a VMX preemption timer while the VP
            // is outside guest mode, so it does not deliver one when the VP
            // stops; it fires the timer again once restored instead.
            if !kvm_may_use_preemption_timer() {
                assert_eq!(saved.irr[BANK], BIT);
            }
            assert_eq!(saved.timer_icr, 30_000_000);
            guest.restore(&saved);
            after = guest.timer_interrupts(Duration::from_millis(200));
        });
        after
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn a_timer_interrupt_that_kvm_holds_is_taken_once_after_a_restore() {
        for idle in [Idle::Halt, Idle::Spin] {
            for expires_before_stop in [true, false] {
                assert_eq!(
                    expired_while_stopped_then_restored(idle, expires_before_stop),
                    1,
                    "{idle:?}, expires before the stop: {expires_before_stop}"
                );
            }
        }
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn an_expiry_that_kvm_records_after_the_stop_is_saved_as_pending() {
        // KVM records an expiry when its host timer fires, which can be a
        // moment after the count reads 0. Stop the VP as the count reads 0,
        // so that KVM may record the expiry only after it first delivers.
        with_guest(Idle::Halt, |guest| {
            let mut late_stops = 0;
            for i in 0..500 {
                guest.arm_timer(3_000_000);
                // A loaded host may stop the VP only after its timer expired.
                if guest
                    .run_for(Duration::from_millis(1))
                    .contains(&TIMER_PORT)
                {
                    late_stops += 1;
                    continue;
                }
                loop {
                    let mut page = [0; 1024];
                    guest.vp.get_lapic(&mut page).unwrap();
                    if ApicRegisters::from_page(&page).timer_ccr == 0 {
                        break;
                    }
                }
                let timer = guest.stop();
                let saved = guest.save(timer);
                if !kvm_may_use_preemption_timer() {
                    assert_eq!(saved.irr[BANK], BIT, "run {i}");
                }
                assert_eq!(saved.timer_icr, 3_000_000, "run {i}");
                guest.restore(&saved);
                assert_eq!(
                    guest.timer_interrupts(Duration::from_millis(10)),
                    1,
                    "run {i}"
                );
            }
            assert!(late_stops < 50, "{late_stops} late stops");
        });
    }

    #[test]
    #[ignore = "requires access to /dev/kvm"]
    fn a_restore_gives_each_timer_expiry_one_interrupt_whenever_the_vp_stops() {
        const TIMER: Duration = Duration::from_millis(2);
        // KVM may hold the interrupt of a VMX preemption timer, so it fires a
        // timer whose interrupt the guest took again.
        let refires = kvm_may_use_preemption_timer();
        let mut seed: u64 = 0x1390;
        let mut random = move |range: Duration| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            range.mul_f64((seed >> 11) as f64 / (1u64 << 53) as f64)
        };
        for idle in [Idle::Halt, Idle::Spin] {
            with_guest(idle, |guest| {
                for i in 0..100 {
                    // Stop within 150 us of the expiry in half of the runs,
                    // and anywhere in twice the timer's period in the others;
                    // then save at once, or after up to 3 ms.
                    let stop_after = if i % 2 == 0 {
                        TIMER - Duration::from_micros(150) + random(Duration::from_micros(300))
                    } else {
                        random(TIMER * 2)
                    };
                    let wait = if i % 4 < 2 {
                        Duration::ZERO
                    } else {
                        random(Duration::from_millis(3))
                    };
                    guest.arm_timer(TIMER.as_nanos() as u32);
                    let before = guest.timer_interrupts(stop_after);
                    let timer = guest.stop();
                    std::thread::sleep(wait);
                    let saved = guest.save(timer);
                    guest.restore(&saved);
                    let after = guest.timer_interrupts(Duration::from_millis(30));
                    let total = before + after;
                    assert!(
                        total == 1 || (refires && total == 2),
                        "run {i}: {before} + {after} timer interrupts, {idle:?}, stopped after \
                         {stop_after:?}, saved after {wait:?}: {timer:?}, saved IRR {:#x}, ISR \
                         {:#x}, initial count {}",
                        saved.irr[BANK],
                        saved.isr[BANK],
                        saved.timer_icr,
                    );
                }
            });
        }
    }
}
