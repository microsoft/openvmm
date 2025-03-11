// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bitfield_struct::bitfield;
use chipset_device::ChipsetDevice;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use inspect::Inspect;
use inspect::InspectMut;
use open_enum::open_enum;
use std::ops::RangeInclusive;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;
use thiserror::Error;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;

#[rustfmt::skip]
#[derive(Inspect)]
#[bitfield(u8)]
struct ControlWord {
    #[bits(1)] bcd: bool,
    #[inspect(with = "|x| Mode::from(*x)")]
    #[bits(3)] mode: u8,
    #[inspect(with = "|x| RwMode(*x)")]
    #[bits(2)] rw: u8,
    #[inspect(skip)] // Ignore `select` since it's not part of the persistent state.
    #[bits(2)] select: u8,
}

#[rustfmt::skip]
#[bitfield(u8)]
struct StatusWord {
    #[bits(1)] bcd: bool,
    #[bits(3)] mode: u8,
    #[bits(2)] rw: u8,
    #[bits(1)] null: bool,
    #[bits(1)] out: bool,
}

#[bitfield(u8)]
struct ReadBackCommand {
    reserved: bool,
    counter0: bool,
    counter1: bool,
    counter2: bool,
    status: bool,
    count: bool,
    #[bits(2)]
    one: u8,
}

const PIT_TIMER_RANGE_START: u16 = 0x40;
const PIT_TIMER_RANGE_END: u16 = 0x42;
const PIT_CONTROL_REGISTER: u16 = 0x43;
const PIT_PORT61_REGISTER: u16 = 0x61;

#[derive(Debug, Inspect)]
struct Timer {
    // Static configuration
    enabled_at_reset: bool,

    // Runtime glue
    interrupt: Option<LineInterrupt>,

    // Volatile state
    #[inspect(flatten)]
    state: TimerState,
}

#[derive(Copy, Clone, Debug, Inspect)]
struct TimerState {
    ce: u16,         // "counting element", i.e. the counter
    cr: u16,         // count register, the new value
    ol: Option<u16>, // the output latch
    sl: Option<u8>,  // the status latch
    state: CountState,
    control: ControlWord,
    out: bool,       // timer output
    gate: bool,      // timer input
    null: bool,      // cr has been set but not copied to ce yet
    read_high: bool, // read the high counter byte next
    cr_low: Option<u8>,
}

#[derive(Copy, Clone, Debug, Inspect, PartialEq, Eq)]
enum CountState {
    Inactive,
    WaitingForGate,
    Reloading,
    Active,
    Counting,
}

#[derive(Debug, Copy, Clone, Inspect, PartialEq, Eq)]
enum Mode {
    TerminalCount = 0,
    HardwareOneShot = 1,
    RateGenerator = 2,
    SquareWave = 3,
    SoftwareStrobe = 4,
    HardwareStrobe = 5,
}

impl From<u8> for Mode {
    fn from(v: u8) -> Self {
        match v {
            0 => Mode::TerminalCount,
            1 => Mode::HardwareOneShot,
            2 | 6 => Mode::RateGenerator,
            3 | 7 => Mode::SquareWave,
            4 => Mode::SoftwareStrobe,
            5 => Mode::HardwareStrobe,
            _ => unreachable!(),
        }
    }
}

impl Mode {
    /// Returns true for modes where counting stops when gate is low.
    fn gate_stops_count(&self) -> bool {
        match self {
            Mode::TerminalCount | Mode::RateGenerator | Mode::SquareWave | Mode::SoftwareStrobe => {
                true
            }
            Mode::HardwareOneShot | Mode::HardwareStrobe => false,
        }
    }
}

open_enum! {
    #[derive(Inspect)]
    #[inspect(debug)]
    enum RwMode: u8 {
        LOW = 1,
        HIGH = 2,
        LOW_HIGH = 3,
    }
}

const fn from_bcd(n: u16) -> u16 {
    (n & 0xf) + ((n & 0xf0) >> 4) * 10 + ((n & 0xf00) >> 8) * 100 + ((n & 0xf000) >> 12) * 1000
}

const fn to_bcd(n: u16) -> u16 {
    (n % 10) + (((n / 10) % 10) << 4) + (((n / 100) % 10) << 8) + (((n / 1000) % 10) << 12)
}

/// Subtracts `n` from `ce` with wrap.
///
/// If `bcd`, then `ce` is in BCD format. The return value is always in binary
/// format.
fn counter_sub(ce: u16, n: u64, bcd: bool) -> u64 {
    if bcd {
        let ce = from_bcd(ce);
        let n = (n % 10000) as u16;
        (if ce >= n { ce - n } else { 10000 - (n - ce) }) as u64
    } else {
        ce.wrapping_sub(n as u16) as u64
    }
}

/// Nanoseconds per PIT tick.
const NANOS_PER_TICK: u64 = 838;

impl Timer {
    fn new(enabled_at_reset: bool, interrupt: Option<LineInterrupt>) -> Self {
        Self {
            enabled_at_reset,
            interrupt,
            state: TimerState::new(enabled_at_reset),
        }
    }

    fn reset(&mut self) {
        self.state = TimerState::new(self.enabled_at_reset);
        self.sync_interrupt();
    }

    fn sync_interrupt(&mut self) {
        if let Some(interrupt) = &self.interrupt {
            interrupt.set_level(self.state.out);
        }
    }

    fn set_out(&mut self, state: bool) {
        if self.state.out != state {
            self.state.out = state;
            self.sync_interrupt();
        }
    }

    fn load_ce(&mut self) {
        self.state.ce = self.state.cr;
        self.state.null = false;
    }

    /// Sets CE to the given value, wrapped. Stores CE in BCD
    /// format if the PIT is in BCD mode.
    fn set_ce(&mut self, ce: u64) {
        if self.state.control.bcd() {
            self.state.ce = to_bcd((ce % 10000) as u16);
        } else {
            self.state.ce = ce as u16;
        }
    }

    fn evaluate(&mut self, mut ticks: u64) {
        let mode = self.state.op_mode();
        let bcd = self.state.control.bcd();
        while ticks > 0 {
            match self.state.state {
                CountState::Inactive | CountState::WaitingForGate => break,
                CountState::Reloading => {
                    ticks -= 1;
                    self.load_ce();
                    self.state.state = CountState::Active;
                }
                CountState::Active => {
                    if !self.state.gate && mode.gate_stops_count() {
                        break;
                    }
                    // Counts per tick.
                    let per = match mode {
                        Mode::TerminalCount
                        | Mode::HardwareOneShot
                        | Mode::RateGenerator
                        | Mode::SoftwareStrobe
                        | Mode::HardwareStrobe => 1,
                        Mode::SquareWave => {
                            // Strip the low bit. This takes an extra tick when
                            // out is high.
                            if self.state.ce & 1 != 0 {
                                self.state.ce &= !1;
                                if self.state.out {
                                    ticks -= 1;
                                    continue;
                                }
                            }
                            2
                        }
                    };
                    if self.state.ce as u64 == per {
                        // Terminal state.
                        self.state.ce = 0;
                        ticks -= 1;
                        match mode {
                            Mode::TerminalCount | Mode::HardwareOneShot => {
                                self.set_out(true);
                                self.state.state = CountState::Counting;
                            }
                            Mode::RateGenerator => {
                                self.set_out(true);
                                self.load_ce();
                            }
                            Mode::SquareWave => {
                                self.set_out(!self.state.out);
                                self.load_ce();
                            }
                            Mode::SoftwareStrobe | Mode::HardwareStrobe => {
                                self.set_out(false);
                                self.state.state = CountState::Counting;
                            }
                        }
                    } else {
                        if ticks >= counter_sub(self.state.ce, per, bcd) / per {
                            // Decrement down to one tick before the terminal state.
                            ticks -= counter_sub(self.state.ce, per, bcd) / per;
                            self.state.ce = per as u16;
                            if mode == Mode::RateGenerator {
                                self.set_out(false);
                            }
                        } else {
                            self.set_ce(counter_sub(self.state.ce, ticks * per, bcd));
                            ticks = 0;
                        }
                    }
                }
                CountState::Counting => {
                    if !self.state.gate && mode.gate_stops_count() {
                        break;
                    }
                    self.set_ce(counter_sub(self.state.ce, ticks, bcd));
                    ticks = 0;
                    self.set_out(true);
                }
            }
        }
    }

    fn set_control(&mut self, control: ControlWord) {
        if control.rw() == 0 {
            self.state.latch_counter();
            return;
        }

        self.state.control = control.with_select(0);
        self.state.ce = 0;
        self.state.cr = 0;
        self.state.cr_low = None;
        self.state.read_high = false;
        self.state.state = CountState::Inactive;
        self.state.null = true;
        self.set_out(match self.state.op_mode() {
            Mode::TerminalCount => false,
            Mode::HardwareOneShot => true,
            Mode::RateGenerator | Mode::SquareWave => true,
            Mode::SoftwareStrobe => true,
            Mode::HardwareStrobe => true,
        });
    }

    fn write(&mut self, n: u8) {
        let n = n as u16;
        match RwMode(self.state.control.rw()) {
            RwMode::LOW => self.state.cr = n,
            RwMode::HIGH => self.state.cr = n << 8,
            RwMode::LOW_HIGH => {
                if let Some(low) = self.state.cr_low {
                    self.state.cr = (n << 8) | (low as u16);
                } else {
                    self.state.cr_low = Some(n as u8);
                    // Wait for high to be set before taking any actions.
                    return;
                }
            }
            _ => unreachable!(),
        }
        self.state.null = true;
        match self.state.op_mode() {
            Mode::TerminalCount => {
                self.state.state = CountState::Reloading;
                self.set_out(false);
            }
            Mode::HardwareOneShot => {
                self.state.state = CountState::WaitingForGate;
            }
            Mode::RateGenerator | Mode::SquareWave => {
                if self.state.state != CountState::Active {
                    self.state.state = CountState::Reloading;
                }
            }
            Mode::SoftwareStrobe => {
                self.state.state = CountState::Reloading;
            }
            Mode::HardwareStrobe => {
                self.state.state = CountState::WaitingForGate;
            }
        }
    }

    fn read(&mut self) -> u8 {
        if let Some(sl) = self.state.sl.take() {
            return sl;
        }
        let value = self.state.ol.unwrap_or(self.state.ce);
        let value = match RwMode(self.state.control.rw()) {
            RwMode::LOW => value as u8,
            RwMode::HIGH => (value >> 8) as u8,
            RwMode::LOW_HIGH => {
                self.state.read_high = !self.state.read_high;
                if self.state.read_high {
                    value as u8
                } else {
                    (value >> 8) as u8
                }
            }
            _ => unreachable!(),
        };
        if !self.state.read_high {
            self.state.ol = None;
        }
        value
    }

    fn set_gate(&mut self, gate: bool) {
        match self.state.op_mode() {
            Mode::TerminalCount => {}
            Mode::HardwareOneShot => {
                if !self.state.gate && gate && self.state.state == CountState::WaitingForGate {
                    self.state.state = CountState::Reloading;
                    self.set_out(false);
                }
            }
            Mode::RateGenerator | Mode::SquareWave => {
                if gate && !self.state.gate {
                    if self.state.state == CountState::Active {
                        self.state.state = CountState::Reloading;
                    }
                } else if !gate {
                    self.set_out(true);
                }
            }
            Mode::SoftwareStrobe => {}
            Mode::HardwareStrobe => {
                if !self.state.gate && gate && self.state.state == CountState::WaitingForGate {
                    self.state.state = CountState::Reloading;
                }
            }
        }
        self.state.gate = gate;
    }
}

impl TimerState {
    fn new(enabled: bool) -> Self {
        Self {
            ce: 0,
            cr: 0,
            ol: None,
            sl: None,
            control: ControlWord::new().with_rw(1),
            state: CountState::Inactive,
            out: false,
            null: true,
            gate: enabled,
            read_high: false,
            cr_low: None,
        }
    }

    fn op_mode(&self) -> Mode {
        self.control.mode().into()
    }

    // Returns the number of ticks until the next interrupt will occur.
    fn next_wakeup(&self) -> Option<u64> {
        let mode = self.op_mode();
        let bcd = self.control.bcd();
        match self.state {
            CountState::Inactive => None,
            CountState::WaitingForGate => None,
            CountState::Reloading | CountState::Active => {
                if !self.gate && mode.gate_stops_count() {
                    return None;
                }
                // Add an extra count for the reload cycle.
                let (ce, extra) = if self.state == CountState::Reloading {
                    (self.cr, 1)
                } else {
                    (self.ce, 0)
                };
                let v = {
                    match mode {
                        Mode::TerminalCount
                        | Mode::HardwareOneShot
                        | Mode::SoftwareStrobe
                        | Mode::HardwareStrobe => {
                            // Changing output in ce ticks.
                            counter_sub(ce, 1, bcd) + 1
                        }
                        Mode::RateGenerator => {
                            if ce == 1 {
                                // Going high in 1 tick.
                                1
                            } else {
                                // Going low in ce - 1 ticks.
                                counter_sub(ce, 1, bcd)
                            }
                        }
                        Mode::SquareWave => {
                            // Inverts in ce / 2 ticks, rounding up if out is high.
                            (counter_sub(ce, 2, bcd) + 2) / 2 + (self.out && ce & 1 != 0) as u64
                        }
                    }
                };
                Some(v + extra)
            }
            CountState::Counting => {
                if self.out || (!self.gate && mode.gate_stops_count()) {
                    None
                } else {
                    Some(1)
                }
            }
        }
    }

    fn latch_status(&mut self) {
        if self.sl.is_none() {
            self.sl = Some(
                StatusWord(self.control.0)
                    .with_null(self.null)
                    .with_out(self.out)
                    .into(),
            );
        }
    }

    fn latch_counter(&mut self) {
        if self.ol.is_none() {
            self.ol = Some(self.ce);
        }
    }
}

#[derive(InspectMut)]
pub struct PitDevice {
    // Runtime glue
    vmtime: VmTimeAccess,

    // Sub-emulators
    #[inspect(iter_by_index)]
    timers: [Timer; { PIT_TIMER_RANGE_END - PIT_TIMER_RANGE_START + 1 } as usize],

    // Runtime book-keeping
    dram_refresh: bool, // just jitters back and forth

    // Volatile state
    last: VmTime,
}

impl PitDevice {
    pub fn new(interrupt: LineInterrupt, vmtime: VmTimeAccess) -> Self {
        PitDevice {
            // Timers 1 and 2 are enabled by default. Timer 1's output is hooked
            // up to the interrupt line.
            timers: [
                Timer::new(true, Some(interrupt)),
                Timer::new(true, None),
                Timer::new(false, None),
            ],
            last: vmtime.now(),
            vmtime,
            dram_refresh: false,
        }
    }

    fn evaluate(&mut self, now: VmTime) {
        // Accumulate an integer number of ticks.
        //
        // N.B. if self.last were set to now, then each call to evaluate
        // would leak a portion of a tick, causing timers to expire late.
        let delta = now.checked_sub(self.last).unwrap_or(Duration::ZERO);
        let ticks = delta.as_nanos() as u64 / NANOS_PER_TICK;
        self.last = self
            .last
            .wrapping_add(Duration::from_nanos(ticks * NANOS_PER_TICK));
        self.timers[0].evaluate(ticks);
        self.timers[1].evaluate(ticks);
        self.timers[2].evaluate(ticks);
    }

    fn arm_wakeup(&mut self) {
        // Request another tick if needed. This is only needed for timer 0 since
        // that's the only one wired up to an interrupt.
        if let Some(next) = self.timers[0].state.next_wakeup() {
            // Delay waking up if the next wakeup is too soon to avoid spinning.
            let next = next.max(20);
            self.vmtime.set_timeout_if_before(
                self.last
                    .wrapping_add(Duration::from_nanos(next * NANOS_PER_TICK)),
            );
        }
    }
}

impl ChangeDeviceState for PitDevice {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        for timer in &mut self.timers {
            timer.reset();
        }
        self.last = self.vmtime.now();
    }
}

impl ChipsetDevice for PitDevice {
    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }

    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }
}

impl PollDevice for PitDevice {
    fn poll_device(&mut self, cx: &mut Context<'_>) {
        if let Poll::Ready(now) = self.vmtime.poll_timeout(cx) {
            self.evaluate(now);
            // Re-register the poll before arming the next wakeup rather than
            // after so that a very short wakeup will still allow this function
            // to return, hopefully avoiding livelock.
            assert!(self.vmtime.poll_timeout(cx).is_pending());
            self.arm_wakeup();
        }
    }
}

impl PortIoIntercept for PitDevice {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        if data.len() != 1 {
            return IoResult::Err(IoError::InvalidAccessSize);
        }

        self.evaluate(self.vmtime.now());
        match io_port {
            PIT_TIMER_RANGE_START..=PIT_TIMER_RANGE_END => {
                let offset = io_port - PIT_TIMER_RANGE_START;
                data[0] = self.timers[offset as usize].read();
            }
            PIT_CONTROL_REGISTER => {
                tracelimit::warn_ratelimited!("reading from write-only command register!");
                data[0] = !0;
            }
            PIT_PORT61_REGISTER => {
                data[0] = ((self.timers[2].state.out as u8) << 5)
                    | ((self.dram_refresh as u8) << 4)
                    | self.timers[2].state.gate as u8;
                // Cycle the DRAM refresh bit every read. PCAT uses this to
                // validate that DRAM is working, but it's not practical or
                // useful to make the timing accurate.
                self.dram_refresh = !self.dram_refresh;
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        self.arm_wakeup();
        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        let &[b] = data else {
            return IoResult::Err(IoError::InvalidAccessSize);
        };

        self.evaluate(self.vmtime.now());

        match io_port {
            PIT_TIMER_RANGE_START..=PIT_TIMER_RANGE_END => {
                let offset = io_port - PIT_TIMER_RANGE_START;
                self.timers[offset as usize].write(b);
            }
            PIT_CONTROL_REGISTER => {
                let control = ControlWord(b);
                match control.select() {
                    i @ 0..=2 => {
                        tracing::trace!(timer = i, ?control, "control write");
                        self.timers[i as usize].set_control(control);
                    }
                    3 => {
                        let command = ReadBackCommand(b);
                        tracing::trace!(?command, "read back");
                        for (i, select) in
                            [command.counter0(), command.counter1(), command.counter2()]
                                .into_iter()
                                .enumerate()
                        {
                            if select {
                                if command.status() {
                                    self.timers[i].state.latch_status();
                                }
                                if command.count() {
                                    self.timers[i].state.latch_counter();
                                }
                            }
                        }
                    }
                    _ => unreachable!(),
                }
            }
            PIT_PORT61_REGISTER => {
                self.timers[2].set_gate((b & 1) != 0);
            }
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        self.arm_wakeup();
        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[
            ("main", PIT_TIMER_RANGE_START..=PIT_CONTROL_REGISTER),
            ("port61", PIT_PORT61_REGISTER..=PIT_PORT61_REGISTER),
        ]
    }
}

mod save_restore {
    use super::*;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    mod state {
        use mesh::payload::Protobuf;
        use vmcore::save_restore::SavedStateRoot;
        use vmcore::vmtime::VmTime;

        #[derive(Protobuf)]
        #[mesh(package = "chipset.pit")]
        pub enum SavedCountState {
            #[mesh(1)]
            Inactive,
            #[mesh(2)]
            WaitingForGate,
            #[mesh(3)]
            Reloading,
            #[mesh(4)]
            Active,
            #[mesh(5)]
            Counting,
        }

        #[derive(Protobuf)]
        #[mesh(package = "chipset.pit")]
        pub struct SavedTimerState {
            #[mesh(1)]
            pub ce: u16,
            #[mesh(2)]
            pub cr: u16,
            #[mesh(3)]
            pub ol: Option<u16>,
            #[mesh(4)]
            pub sl: Option<u8>,
            #[mesh(5)]
            pub state: SavedCountState,
            #[mesh(6)]
            pub control: u8,
            #[mesh(7)]
            pub out: bool,
            #[mesh(8)]
            pub gate: bool,
            #[mesh(9)]
            pub null: bool,
            #[mesh(10)]
            pub read_high: bool,
            #[mesh(11)]
            pub cr_low: Option<u8>,
        }

        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.pit")]
        pub struct SavedState {
            #[mesh(1)]
            pub timers: [SavedTimerState; 3],
            #[mesh(2)]
            pub last: VmTime,
        }
    }

    #[derive(Debug, Error)]
    enum PitDeviceRestoreError {
        #[error("last tick time is after current time")]
        InvalidLastTick,
    }

    impl SaveRestore for PitDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let Self {
                vmtime: _,
                timers,
                dram_refresh: _,
                last,
            } = self;

            Ok(state::SavedState {
                timers: [&timers[0].state, &timers[1].state, &timers[2].state].map(|timer| {
                    let &TimerState {
                        ce,
                        cr,
                        ol,
                        sl,
                        state,
                        control,
                        out,
                        gate,
                        null,
                        read_high,
                        cr_low,
                    } = timer;

                    state::SavedTimerState {
                        ce,
                        cr,
                        ol,
                        sl,
                        state: match state {
                            CountState::Inactive => state::SavedCountState::Inactive,
                            CountState::WaitingForGate => state::SavedCountState::WaitingForGate,
                            CountState::Reloading => state::SavedCountState::Reloading,
                            CountState::Active => state::SavedCountState::Active,
                            CountState::Counting => state::SavedCountState::Counting,
                        },
                        control: control.into(),
                        out,
                        gate,
                        null,
                        read_high,
                        cr_low,
                    }
                }),

                last: *last,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { timers, last } = state;

            for (timer, state) in self.timers.iter_mut().zip(timers) {
                let state::SavedTimerState {
                    ce,
                    cr,
                    ol,
                    sl,
                    state,
                    control,
                    out,
                    gate,
                    null,
                    read_high,
                    cr_low,
                } = state;

                timer.state = TimerState {
                    ce,
                    cr,
                    ol,
                    sl,
                    state: match state {
                        state::SavedCountState::Inactive => CountState::Inactive,
                        state::SavedCountState::WaitingForGate => CountState::WaitingForGate,
                        state::SavedCountState::Reloading => CountState::Reloading,
                        state::SavedCountState::Active => CountState::Active,
                        state::SavedCountState::Counting => CountState::Counting,
                    },
                    out,
                    control: ControlWord::from(control), // no unused bits
                    gate,
                    null,
                    read_high,
                    cr_low,
                };

                timer.sync_interrupt();
            }

            self.last = last;
            if last.is_after(self.vmtime.now()) {
                return Err(RestoreError::InvalidSavedState(
                    PitDeviceRestoreError::InvalidLastTick.into(),
                ));
            }

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ControlWord;
    use super::Mode;
    use super::RwMode;
    use super::Timer;
    use super::to_bcd;
    use crate::pit::from_bcd;

    #[test]
    fn test_bcd_comp() {
        for i in 0..=9999 {
            assert_eq!(from_bcd(to_bcd(i)), i, "{i} {}", to_bcd(i));
        }
    }

    fn set_timer(timer: &mut Timer, mode: Mode, mut cr: u16, bcd: bool) {
        timer.set_control(
            ControlWord::new()
                .with_mode(mode as u8)
                .with_rw(RwMode::LOW_HIGH.0)
                .with_bcd(bcd),
        );
        if bcd {
            cr = to_bcd(cr);
        }
        timer.write(cr as u8);
        timer.write((cr >> 8) as u8);
    }

    fn check_invert(timer: &mut Timer, initial_out: bool, expected_next: u64) {
        let mode = Mode::from(timer.state.control.mode());
        assert_eq!(timer.state.out, initial_out, "{mode:?}");
        let n = timer.state.next_wakeup().unwrap();
        assert_eq!(n, expected_next, "{mode:?}");
        for i in 0..n - 1 {
            timer.evaluate(1);
            assert_eq!(
                i + timer.state.next_wakeup().unwrap() + 1,
                n,
                "{mode:?}, {i}"
            );
            assert_eq!(timer.state.out, initial_out, "{mode:?}, {i}");
        }
        timer.evaluate(1);
        assert_eq!(timer.state.out, !initial_out, "{mode:?}, {n}");
    }

    fn check_done(timer: &mut Timer) {
        assert!(timer.state.next_wakeup().is_none());
        let out = timer.state.out;
        for _ in 0..65536 {
            timer.evaluate(1);
            assert_eq!(timer.state.out, out);
        }
    }

    fn test_output(bcd: bool) {
        let mut timer = Timer::new(true, None);
        let max = if bcd { 10000 } else { 0x10000 };

        set_timer(&mut timer, Mode::TerminalCount, 0, bcd);
        check_invert(&mut timer, false, max + 1);
        check_done(&mut timer);

        set_timer(&mut timer, Mode::HardwareOneShot, 0, bcd);
        check_done(&mut timer);
        timer.set_gate(false);
        timer.set_gate(true);
        check_invert(&mut timer, false, max + 1);
        check_done(&mut timer);

        set_timer(&mut timer, Mode::RateGenerator, 0, bcd);
        check_invert(&mut timer, true, max);
        check_invert(&mut timer, false, 1);
        check_invert(&mut timer, true, max - 1);

        set_timer(&mut timer, Mode::SquareWave, 0, bcd);
        check_invert(&mut timer, true, max / 2 + 1);
        check_invert(&mut timer, false, max / 2);
        check_invert(&mut timer, true, max / 2);

        set_timer(&mut timer, Mode::SquareWave, 1001, bcd);
        check_invert(&mut timer, true, 502);
        check_invert(&mut timer, false, 500);
        check_invert(&mut timer, true, 501);

        set_timer(&mut timer, Mode::SoftwareStrobe, 0, bcd);
        check_invert(&mut timer, true, max + 1);
        check_invert(&mut timer, false, 1);
        check_done(&mut timer);

        set_timer(&mut timer, Mode::HardwareStrobe, 0, bcd);
        check_done(&mut timer);
        timer.set_gate(false);
        timer.set_gate(true);
        check_invert(&mut timer, true, max + 1);
        check_invert(&mut timer, false, 1);
        check_done(&mut timer);
    }

    #[test]
    fn test_binary() {
        test_output(false);
    }

    #[test]
    fn test_bcd() {
        test_output(true);
    }
}
