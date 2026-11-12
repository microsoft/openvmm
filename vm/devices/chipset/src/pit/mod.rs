// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod resolver;

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

impl PartialEq for ControlWord {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for ControlWord {}

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

#[derive(Copy, Clone, Debug, Inspect, PartialEq, Eq)]
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
                    if self.state.ce == self.state.cr
                        && !self.state.null
                        && matches!(mode, Mode::RateGenerator | Mode::SquareWave)
                    {
                        let reload = if bcd {
                            u64::from(from_bcd(self.state.cr))
                        } else {
                            u64::from(self.state.cr)
                        };
                        if reload == 1 && mode == Mode::RateGenerator {
                            // A count-one rate generator reloads every tick without
                            // lowering OUT.
                            break;
                        }
                        let period = if reload == 0 {
                            if bcd { 10_000 } else { 0x1_0000 }
                        } else if reload == 1 {
                            // Mode 3 strips the odd bit before wrapping through the
                            // counter range, taking one extra tick to return here.
                            if bcd { 10_001 } else { 0x1_0001 }
                        } else {
                            reload
                        };
                        if ticks >= period {
                            // Skip whole cycles, preserving phase and emitting one
                            // representative pulse instead of replaying every transition.
                            let out = self.state.out;
                            self.set_out(!out);
                            self.set_out(out);
                            ticks %= period;
                            if ticks == 0 {
                                break;
                            }
                        }
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
                // The high byte completes the count and returns the write
                // flip-flop to the low byte of the next count.
                if let Some(low) = self.state.cr_low.take() {
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

    // Static configuration
    time_abi: bool,

    // Volatile state
    /// The VM time of the last counted tick, rounded down to VM time's 100 ns
    /// resolution.
    last: VmTime,
    /// The nanoseconds, below 100, by which the last counted tick follows
    /// `last`.
    last_frac_ns: u32,
}

/// Channel 0 counts in a periodic mode, which the NVX time ABI forbids at
/// snapshot capture and restore.
#[derive(Debug, Error)]
#[error("[E_PIT_ACTIVE] PIT channel 0 is counting in periodic mode {mode:?}")]
struct PitActiveError {
    mode: Mode,
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
            last_frac_ns: 0,
            vmtime,
            dram_refresh: false,
            time_abi: false,
        }
    }

    /// Applies the NVX time ABI: saving or restoring the device while
    /// channel 0 counts in a periodic mode fails with `E_PIT_ACTIVE`.
    pub fn with_time_abi(mut self) -> Self {
        self.time_abi = true;
        self
    }

    /// Fails if the time ABI applies and channel 0 counts in a periodic mode.
    fn check_time_abi(&self) -> Result<(), PitActiveError> {
        let state = &self.timers[0].state;
        let mode = state.op_mode();
        if self.time_abi
            && matches!(mode, Mode::RateGenerator | Mode::SquareWave)
            && state.state != CountState::Inactive
        {
            return Err(PitActiveError { mode });
        }
        Ok(())
    }

    fn evaluate(&mut self, now: VmTime) {
        // Accumulate an integer number of ticks.
        //
        // N.B. if self.last were set to now, then each call to evaluate
        // would leak a portion of a tick, causing timers to expire late.
        // Ticks generally fall between VM time's 100 ns steps, so the last
        // counted tick lies `last_frac_ns` past `last`: dropping that part
        // would count it again in the next call, causing timers to expire
        // early, and the more so the more often the guest accesses the PIT.
        let delta = now.checked_sub(self.last).unwrap_or(Duration::ZERO);
        let ticks =
            (delta.as_nanos() as u64).saturating_sub(self.last_frac_ns.into()) / NANOS_PER_TICK;
        let counted = u64::from(self.last_frac_ns) + ticks * NANOS_PER_TICK;
        // `wrapping_add` adds the whole 100 ns steps of the counted time.
        self.last = self.last.wrapping_add(Duration::from_nanos(counted));
        self.last_frac_ns = (counted % 100) as u32;
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
            // The timeout fires once VM time passes it, so rounding it down to
            // VM time's 100 ns resolution still wakes up at or after the tick.
            self.vmtime
                .set_timeout_if_before(self.last.wrapping_add(Duration::from_nanos(
                    u64::from(self.last_frac_ns) + next * NANOS_PER_TICK,
                )));
        }
    }
}

impl ChangeDeviceState for PitDevice {
    fn start(&mut self) {
        self.evaluate(self.vmtime.now());
        self.arm_wakeup();
    }

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        for timer in &mut self.timers {
            timer.reset();
        }
        self.last = self.vmtime.now();
        self.last_frac_ns = 0;
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
                                if !command.status() {
                                    self.timers[i].state.latch_status();
                                }
                                if !command.count() {
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
            /// The nanoseconds, below 100, by which the last counted tick
            /// follows `last`. A saved state without it restores with zero.
            #[mesh(3)]
            pub last_frac_ns: u32,
        }
    }

    #[derive(Debug, Error)]
    enum PitDeviceRestoreError {
        #[error("last tick time is after current time")]
        InvalidLastTick,
        #[error("last tick time fraction {0} ns is not below 100 ns")]
        InvalidLastTickFraction(u32),
        #[error("timer {timer} control word {control:#04x} has no read/write mode")]
        NoReadWriteMode { timer: usize, control: u8 },
    }

    impl SaveRestore for PitDevice {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            self.check_time_abi()
                .map_err(|err| SaveError::Other(err.into()))?;
            let Self {
                vmtime: _,
                timers,
                dram_refresh: _,
                time_abi: _,
                last,
                last_frac_ns,
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
                last_frac_ns: *last_frac_ns,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState {
                timers,
                last,
                last_frac_ns,
            } = state;
            if last_frac_ns >= 100 {
                return Err(RestoreError::InvalidSavedState(
                    PitDeviceRestoreError::InvalidLastTickFraction(last_frac_ns).into(),
                ));
            }

            // Check every timer before changing any state. RW = 0 encodes the
            // counter latch command, which `Timer::set_control` never stores
            // and `Timer::read` and `Timer::write` cannot handle.
            for (timer, saved) in timers.iter().enumerate() {
                if ControlWord::from(saved.control).rw() == 0 {
                    return Err(RestoreError::InvalidSavedState(
                        PitDeviceRestoreError::NoReadWriteMode {
                            timer,
                            control: saved.control,
                        }
                        .into(),
                    ));
                }
            }

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
            self.last_frac_ns = last_frac_ns;
            // The last tick, `last_frac_ns` past `last`, cannot follow now.
            let now = self.vmtime.now();
            if last.is_after(now) || (last == now && last_frac_ns != 0) {
                return Err(RestoreError::InvalidSavedState(
                    PitDeviceRestoreError::InvalidLastTick.into(),
                ));
            }
            self.check_time_abi()
                .map_err(|err| RestoreError::InvalidSavedState(err.into()))?;

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ControlWord;
    use super::Mode;
    use super::NANOS_PER_TICK;
    use super::PIT_CONTROL_REGISTER;
    use super::PIT_PORT61_REGISTER;
    use super::PIT_TIMER_RANGE_START;
    use super::RwMode;
    use super::Timer;
    use super::to_bcd;
    use crate::pit::from_bcd;
    use chipset_device::pio::PortIoIntercept;
    use chipset_device::poll_device::PollDevice;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::task::Context;
    use std::task::Waker;
    use test_with_tracing::test;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::line_interrupt::LineInterrupt;
    use vmcore::line_interrupt::LineSetTarget;
    use vmcore::line_interrupt::test_helpers::TestLineInterruptTarget;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vmtime::VmTime;

    #[derive(Default)]
    struct RecordingTarget(Mutex<Vec<bool>>);

    impl LineSetTarget for RecordingTarget {
        fn set_irq(&self, _vector: u32, high: bool) {
            self.0.lock().push(high);
        }
    }

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

    fn set_low_byte_timer(timer: &mut Timer, mode: Mode, cr: u8) {
        timer.set_control(
            ControlWord::new()
                .with_mode(mode as u8)
                .with_rw(RwMode::LOW.0),
        );
        timer.write(cr);
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

    #[test]
    fn low_high_access_takes_each_count_low_byte_first() {
        // The high byte of a count returns the write flip-flop to the low
        // byte, so each count that follows without a control word starts with
        // its low byte, whatever the mode.
        for mode in [
            Mode::TerminalCount,
            Mode::HardwareOneShot,
            Mode::RateGenerator,
            Mode::SquareWave,
            Mode::SoftwareStrobe,
            Mode::HardwareStrobe,
        ] {
            let mut timer = Timer::new(true, None);
            set_timer(&mut timer, mode, 0x1234, false);
            for count in [0x5678_u16, 0x9abc] {
                let [low, high] = count.to_le_bytes();
                timer.write(low);
                timer.write(high);
                assert_eq!(timer.state.cr, count, "{mode:?}");
            }
        }
    }

    /// A PIT that a test programs through its I/O ports, as a guest does, in
    /// a VM time that stands still until `run_to` or `set_time` moves it.
    struct GuestPit {
        pit: super::PitDevice,
        irq0: Arc<TestLineInterruptTarget>,
        keeper: vmcore::vmtime::VmTimeKeeper,
        pool: pal_async::DefaultPool,
    }

    impl GuestPit {
        fn new() -> Self {
            use vmcore::vmtime::VmTime;

            let mut pool = pal_async::DefaultPool::new();
            let driver = pool.driver();
            let keeper = vmcore::vmtime::VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
            let source = pool.run_until(keeper.builder().build(&driver)).unwrap();
            let irq0 = TestLineInterruptTarget::new_arc();
            let pit = super::PitDevice::new(
                LineInterrupt::new_with_target("pit", irq0.clone(), 0),
                source.access("pit"),
            );
            Self {
                pit,
                irq0,
                keeper,
                pool,
            }
        }

        fn write(&mut self, port: u16, data: u8) {
            self.pit.io_write(port, &[data]).unwrap();
        }

        fn read(&mut self, port: u16) -> u8 {
            let mut data = 0;
            self.pit
                .io_read(port, std::slice::from_mut(&mut data))
                .unwrap();
            data
        }

        /// Programs counter 0 to count `count` in `mode`, low then high byte.
        fn program_counter0(&mut self, mode: Mode, count: u16) {
            let control = ControlWord::new()
                .with_mode(mode as u8)
                .with_rw(RwMode::LOW_HIGH.0);
            self.write(PIT_CONTROL_REGISTER, control.into());
            for byte in count.to_le_bytes() {
                self.write(PIT_TIMER_RANGE_START, byte);
            }
        }

        /// Moves VM time to `ticks` PIT ticks after the start, rounded up to
        /// VM time's 100 ns.
        fn run_to(&mut self, ticks: u64) {
            self.set_time((ticks * NANOS_PER_TICK).div_ceil(100));
        }

        /// Moves VM time to `time` 100 ns units after the start.
        fn set_time(&mut self, time: u64) {
            use vmcore::vmtime::SavedState;

            let time = VmTime::from_100ns(time);
            self.pool
                .run_until(self.keeper.restore(SavedState::from_vmtime(time)));
        }

        /// Polls the PIT, as when its VM time wakeup fires.
        fn poll(&mut self) {
            self.pit
                .poll_device(&mut Context::from_waker(Waker::noop()));
        }

        /// Latches counter 0, which brings the PIT up to VM time as every
        /// port access does, and reads its count, low byte first.
        fn latched_count(&mut self) -> u16 {
            self.write(PIT_CONTROL_REGISTER, 0x00);
            let mut count = [0; 2];
            for byte in &mut count {
                self.pit
                    .io_read(PIT_TIMER_RANGE_START, std::slice::from_mut(byte))
                    .unwrap();
            }
            u16::from_le_bytes(count)
        }
    }

    #[test]
    fn low_high_access_reloads_a_count_without_a_control_word() {
        // Counter 0, low then high byte, mode 0: the count 0 (65536), then,
        // without a control word, the count 16, which raises IRQ0 on the 17th
        // tick, after the tick that loads it and its 16 counts.
        let mut guest = GuestPit::new();
        guest.write(PIT_CONTROL_REGISTER, 0x30);
        for byte in [0x00, 0x00, 0x10, 0x00] {
            guest.write(PIT_TIMER_RANGE_START, byte);
        }

        guest.run_to(16);
        let count = guest.latched_count();
        assert!(!guest.irq0.is_high(0), "count {count:#06x}");
        guest.run_to(17);
        let count = guest.latched_count();
        assert!(guest.irq0.is_high(0), "count {count:#06x}");
    }

    #[test]
    fn low_high_access_reprograms_a_running_rate_generator() {
        // Counter 0, low then high byte, mode 2: the count 0x1234, then, while
        // it counts and without a control word, the count 0x5678, which the
        // counter loads at the end of the current period.
        let mut guest = GuestPit::new();
        guest.write(PIT_CONTROL_REGISTER, 0x34);
        guest.write(PIT_TIMER_RANGE_START, 0x34);
        guest.write(PIT_TIMER_RANGE_START, 0x12);
        guest.run_to(1);
        assert_eq!(guest.latched_count(), 0x1234);

        guest.write(PIT_TIMER_RANGE_START, 0x78);
        guest.write(PIT_TIMER_RANGE_START, 0x56);
        guest.run_to(1 + 0x1234);
        assert_eq!(guest.latched_count(), 0x5678);
    }

    #[test]
    fn elapsed_interval_matches_incremental_pit_evaluation() {
        for mode in [
            Mode::TerminalCount,
            Mode::RateGenerator,
            Mode::SquareWave,
            Mode::SoftwareStrobe,
        ] {
            let mut elapsed = Timer::new(true, None);
            let mut incremental = Timer::new(true, None);
            set_timer(&mut elapsed, mode, 100, false);
            set_timer(&mut incremental, mode, 100, false);

            elapsed.evaluate(350);
            for _ in 0..350 {
                incremental.evaluate(1);
            }

            assert_eq!(elapsed.state, incremental.state, "{mode:?}");
        }
    }

    #[test]
    fn elapsed_interval_preserves_periodic_phase() {
        let mut timer = Timer::new(true, None);
        set_timer(&mut timer, Mode::RateGenerator, 100, false);

        timer.evaluate(350);

        assert_eq!(timer.state.ce, 51);
        assert!(timer.state.out);
        assert_eq!(timer.state.next_wakeup(), Some(50));
    }

    #[test]
    fn long_elapsed_interval_fast_forwards_periodic_pit() {
        for mode in [Mode::RateGenerator, Mode::SquareWave] {
            for (count, bcd) in [
                (100, false),
                (1234, true),
                (3, false),
                (3, true),
                (1001, false),
                (1001, true),
                (0, false),
                (0, true),
                (1, false),
                (1, true),
            ] {
                let counter_range = if bcd { 10_000 } else { 0x1_0000 };
                let period = if count == 0 {
                    counter_range
                } else if count == 1 && mode == Mode::SquareWave {
                    counter_range + 1
                } else {
                    u64::from(count)
                };

                let mut elapsed = Timer::new(true, None);
                let mut incremental = Timer::new(true, None);
                set_timer(&mut elapsed, mode, count, bcd);
                set_timer(&mut incremental, mode, count, bcd);
                elapsed.evaluate(1);
                incremental.evaluate(1);

                let phased_ticks = period * 2 + period / 3;
                elapsed.evaluate(phased_ticks);
                for _ in 0..phased_ticks {
                    incremental.evaluate(1);
                }
                assert_eq!(
                    elapsed.state, incremental.state,
                    "{mode:?}, count={count}, bcd={bcd}"
                );

                let mut elapsed = Timer::new(true, None);
                let mut remainder = Timer::new(true, None);
                set_timer(&mut elapsed, mode, count, bcd);
                set_timer(&mut remainder, mode, count, bcd);
                elapsed.evaluate(1);
                remainder.evaluate(1);

                elapsed.evaluate(u64::MAX);
                remainder.evaluate(u64::MAX % period);
                assert_eq!(
                    elapsed.state, remainder.state,
                    "{mode:?}, count={count}, bcd={bcd}"
                );
            }
        }
    }

    #[test]
    fn periodic_fast_forward_matches_incremental_from_each_phase() {
        for mode in [Mode::RateGenerator, Mode::SquareWave] {
            for bcd in [false, true] {
                for count in [2, 3, 100, 1001] {
                    let period = u64::from(count);
                    for phase in [0, 1, period / 2, period / 2 + 1, period - 1] {
                        let mut initial = Timer::new(true, None);
                        set_timer(&mut initial, mode, count, bcd);
                        initial.evaluate(1);
                        for _ in 0..phase {
                            initial.evaluate(1);
                        }

                        for ticks in [
                            period - 1,
                            period,
                            period + 1,
                            period * 3 - 1,
                            period * 3,
                            period * 3 + 1,
                        ] {
                            let mut elapsed = Timer::new(true, None);
                            let mut incremental = Timer::new(true, None);
                            elapsed.state = initial.state;
                            incremental.state = initial.state;

                            elapsed.evaluate(ticks);
                            for _ in 0..ticks {
                                incremental.evaluate(1);
                            }

                            assert_eq!(
                                elapsed.state, incremental.state,
                                "{mode:?}, count={count}, bcd={bcd}, phase={phase}, ticks={ticks}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn count_one_rate_generator_does_not_emit_irq0_edges() {
        for bcd in [false, true] {
            let target = Arc::new(RecordingTarget::default());
            let interrupt = LineInterrupt::new_with_target("pit", target.clone(), 0);
            let mut timer = Timer::new(true, Some(interrupt));
            set_timer(&mut timer, Mode::RateGenerator, 1, bcd);
            timer.evaluate(1);
            target.0.lock().clear();
            let active_state = timer.state;

            timer.evaluate(u64::MAX);

            assert_eq!(timer.state, active_state, "bcd={bcd}");
            assert!(target.0.lock().is_empty(), "bcd={bcd}");
        }
    }

    #[test]
    fn reprogrammed_periodic_count_loads_before_fast_forward() {
        for mode in [Mode::RateGenerator, Mode::SquareWave] {
            let mut elapsed = Timer::new(true, None);
            let mut incremental = Timer::new(true, None);
            set_low_byte_timer(&mut elapsed, mode, 100);
            set_low_byte_timer(&mut incremental, mode, 100);
            elapsed.evaluate(1);
            incremental.evaluate(1);
            elapsed.write(100);
            incremental.write(100);

            elapsed.evaluate(100);
            for _ in 0..100 {
                incremental.evaluate(1);
            }

            assert_eq!(elapsed.state, incremental.state, "{mode:?}");
            assert!(!elapsed.state.null, "{mode:?}");
        }

        let target = Arc::new(RecordingTarget::default());
        let interrupt = LineInterrupt::new_with_target("pit", target.clone(), 0);
        let mut timer = Timer::new(true, Some(interrupt));
        set_low_byte_timer(&mut timer, Mode::RateGenerator, 4);
        timer.evaluate(4);
        timer.write(1);
        target.0.lock().clear();
        assert!(timer.state.null);
        assert!(!timer.state.out);

        timer.evaluate(u64::MAX);

        assert!(!timer.state.null);
        assert!(timer.state.out);
        assert_eq!(*target.0.lock(), [true]);
    }

    #[test]
    fn low_gate_pauses_periodic_fast_forward() {
        for mode in [Mode::RateGenerator, Mode::SquareWave] {
            let target = Arc::new(RecordingTarget::default());
            let interrupt = LineInterrupt::new_with_target("pit", target.clone(), 0);
            let mut timer = Timer::new(true, Some(interrupt));
            set_timer(&mut timer, mode, 100, false);
            timer.evaluate(1);
            timer.set_gate(false);
            target.0.lock().clear();
            let paused_state = timer.state;

            timer.evaluate(u64::MAX);

            assert_eq!(timer.state, paused_state, "{mode:?}");
            assert!(target.0.lock().is_empty(), "{mode:?}");
        }
    }

    #[test]
    fn long_elapsed_interval_coalesces_periodic_irq0_edges() {
        let target = Arc::new(RecordingTarget::default());
        let interrupt = LineInterrupt::new_with_target("pit", target.clone(), 0);
        let mut timer = Timer::new(true, Some(interrupt));
        set_timer(&mut timer, Mode::RateGenerator, 2, false);
        timer.evaluate(1);
        target.0.lock().clear();

        timer.evaluate(u64::MAX - 1);

        assert_eq!(*target.0.lock(), [false, true]);
        assert!(timer.state.out);
    }

    #[test]
    fn time_abi_rejects_periodic_channel0_at_save_and_restore() {
        use super::PitDevice;
        use vmcore::save_restore::SaveRestore;
        use vmcore::vmtime::VmTime;

        let mut pool = pal_async::DefaultPool::new();
        let driver = pool.driver();
        let keeper = vmcore::vmtime::VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let source = pool.run_until(keeper.builder().build(&driver)).unwrap();
        let device = |time_abi| {
            let device = PitDevice::new(LineInterrupt::detached(), source.access("pit"));
            if time_abi {
                device.with_time_abi()
            } else {
                device
            }
        };

        // Linux stops channel 0 by programming mode 0, which is accepted.
        let mut stopped = device(true);
        set_timer(&mut stopped.timers[0], Mode::TerminalCount, 0, false);
        let saved = stopped.save().unwrap();

        for mode in [Mode::RateGenerator, Mode::SquareWave] {
            let mut periodic = device(true);
            set_timer(&mut periodic.timers[0], mode, 100, false);
            periodic.timers[0].evaluate(1);
            let Err(err) = periodic.save() else {
                panic!("a periodic channel 0 saved under the time ABI");
            };
            assert!(format!("{err:#}").contains("[E_PIT_ACTIVE]"), "{err:#}");

            // Without the time ABI the same state saves, and restoring it
            // into a time ABI device fails.
            let mut legacy = device(false);
            set_timer(&mut legacy.timers[0], mode, 100, false);
            legacy.timers[0].evaluate(1);
            let saved_periodic = legacy.save().unwrap();
            let err = device(true).restore(saved_periodic).unwrap_err();
            let mut chain = err.to_string();
            let mut source = std::error::Error::source(&err);
            while let Some(error) = source {
                chain = format!("{chain}: {error}");
                source = error.source();
            }
            assert!(chain.contains("[E_PIT_ACTIVE]"), "{chain}");
        }

        device(true).restore(saved).unwrap();
        drop(keeper);
    }

    #[test]
    fn reads_at_one_vm_time_see_one_count() {
        let mut guest = GuestPit::new();
        guest.program_counter0(Mode::RateGenerator, 0x50);

        // By 2.5 us, two ticks have elapsed: the first loads the count, and
        // the second, at 1676 ns, between two 100 ns steps of VM time,
        // decrements it.
        guest.set_time(25);
        assert_eq!(guest.latched_count(), 0x4f);
        assert_eq!(guest.latched_count(), 0x4f);
    }

    #[test]
    fn restored_pit_counts_from_the_saved_tick() {
        let mut guest = GuestPit::new();
        guest.program_counter0(Mode::RateGenerator, 0x50);
        guest.set_time(25);
        assert_eq!(guest.latched_count(), 0x4f);

        let saved = mesh::payload::encode(guest.pit.save().unwrap());
        let mut restored = GuestPit::new();
        restored.set_time(25);
        restored
            .pit
            .restore(mesh::payload::decode(&saved).unwrap())
            .unwrap();
        assert_eq!(restored.latched_count(), 0x4f);

        // A tick every 838 ns from VM time zero: the first loads the count,
        // and each later one decrements it, reloading it after one.
        for time in 26..=1000 {
            guest.set_time(time);
            restored.set_time(time);
            let count = 0x50 - ((time * 100 / 838 - 1) % 0x50) as u16;
            assert_eq!(restored.latched_count(), count, "restored PIT at {time}");
            assert_eq!(guest.latched_count(), count, "saved PIT at {time}");
        }
    }

    #[test]
    fn restore_rejects_an_invalid_last_tick_time() {
        let mut guest = GuestPit::new();
        // Count two ticks, up to 1676 ns.
        guest.set_time(25);
        guest.read(PIT_PORT61_REGISTER);

        // The last tick lies at `last` plus a fraction below 100 ns, and not
        // after the current time.
        for (last, last_frac_ns, valid) in [
            (16, 99, true),
            (16, 100, false),
            (25, 0, true),
            (25, 1, false),
        ] {
            let mut saved = guest.pit.save().unwrap();
            saved.last = VmTime::from_100ns(last);
            saved.last_frac_ns = last_frac_ns;
            assert_eq!(
                guest.pit.restore(saved).is_ok(),
                valid,
                "last {last}, last_frac_ns {last_frac_ns}"
            );
        }
    }

    #[test]
    fn polling_does_not_speed_up_the_pit() {
        let mut polled = GuestPit::new();
        let mut idle = GuestPit::new();
        polled.program_counter0(Mode::RateGenerator, 0xffff);
        idle.program_counter0(Mode::RateGenerator, 0xffff);

        // Evaluate the PIT every 900 ns for 90 ms, as an access to its ports
        // or to port 0x61 does.
        for step in 1..=100_000 {
            polled.pit.evaluate(VmTime::from_100ns(step * 9));
        }

        // 90 ms is 107,398 ticks: the first loads the count, and the other
        // 107,397 count it down 41,862 past one period of 65,535.
        polled.set_time(900_000);
        idle.set_time(900_000);
        assert_eq!(idle.latched_count(), 23_673);
        assert_eq!(polled.latched_count(), 23_673);
    }

    #[test]
    fn reset_pit_counts_ticks_from_the_reset() {
        let mut guest = GuestPit::new();
        // Count two ticks, up to 1676 ns.
        guest.set_time(25);
        guest.read(PIT_PORT61_REGISTER);

        guest.pool.run_until(guest.pit.reset());
        guest.program_counter0(Mode::RateGenerator, 0x50);
        // 1.7 us after the reset, two ticks have elapsed.
        guest.set_time(42);
        assert_eq!(guest.latched_count(), 0x4f);
    }

    #[test]
    fn wakeup_raises_irq0_once_the_output_changes() {
        let mut guest = GuestPit::new();
        // Programming counter 0 at 2.5 us counts two ticks, up to 1676 ns. The
        // third tick loads the count, and the 22nd, at 18,436 ns, between two
        // 100 ns steps of VM time, ends it and raises the output.
        guest.set_time(25);
        guest.program_counter0(Mode::TerminalCount, 19);
        for time in 26..=185 {
            guest.set_time(time);
            guest.poll();
            assert_eq!(guest.irq0.is_high(0), time == 185, "IRQ 0 at {time}");
        }
    }

    /// Returns the PIT's saved state, encoded, to compare whole PITs.
    fn encoded_state(guest: &mut GuestPit) -> Vec<u8> {
        mesh::payload::encode(guest.pit.save().unwrap())
    }

    /// Reads the counter's port, writes 1 to it, and reads it again, and
    /// returns the bytes read.
    fn access_counter(guest: &mut GuestPit, counter: u16) -> [u8; 2] {
        let port = PIT_TIMER_RANGE_START + counter;
        let first = guest.read(port);
        guest.write(port, 1);
        [first, guest.read(port)]
    }

    /// RW = 0 in an 8254 control word is the counter latch command, which
    /// latches a counter's count without programming the counter, so no
    /// counter holds a control word with RW = 0, and `Timer::read` and
    /// `Timer::write` handle only RW = 1, 2, and 3. Restoring a saved state in
    /// which a counter's control word has RW = 0, whatever its other bits,
    /// fails as invalid saved state before it changes the PIT, which then
    /// reads and writes the counter as a PIT that never restored does.
    #[test]
    fn restore_rejects_rw0_control_word_without_changing_state() {
        // A PIT at 0.1 s, counted up to its last tick. When `programmed`, each
        // of its counters holds state other than a fresh PIT's, so that
        // installing any part of a fresh PIT's saved state shows.
        let new_guest = |programmed: bool| {
            let mut guest = GuestPit::new();
            guest.set_time(1_000_000);
            guest.read(PIT_PORT61_REGISTER);
            if programmed {
                for (port, data) in [
                    // Counter 0: LSB then MSB, mode 2, binary.
                    (PIT_CONTROL_REGISTER, 0x34),
                    (PIT_TIMER_RANGE_START, 0x34),
                    (PIT_TIMER_RANGE_START, 0x12),
                    // Counter 1: LSB only, mode 3, BCD.
                    (PIT_CONTROL_REGISTER, 0x57),
                    (PIT_TIMER_RANGE_START + 1, 0x50),
                    // Counter 2: gate high, then MSB only, mode 1, binary.
                    (PIT_PORT61_REGISTER, 0x01),
                    (PIT_CONTROL_REGISTER, 0xa2),
                    (PIT_TIMER_RANGE_START + 2, 0x20),
                ] {
                    guest.write(port, data);
                }
            }
            guest
        };

        for programmed in [false, true] {
            for counter in 0..3 {
                for control in (0..=u8::MAX).filter(|control| control & 0x30 == 0) {
                    let case = format!(
                        "programmed {programmed}, counter {counter}, control word {control:#04x}"
                    );
                    // A fresh PIT's saved state, from VM time 0, in which the
                    // counter's control word has RW = 0.
                    let mut saved = GuestPit::new().pit.save().unwrap();
                    saved.timers[usize::from(counter)].control = control;

                    let mut guest = new_guest(programmed);
                    let result = guest.pit.restore(saved);
                    assert!(
                        matches!(result, Err(RestoreError::InvalidSavedState(_))),
                        "{case}: {result:?}"
                    );

                    let mut unrestored = new_guest(programmed);
                    assert_eq!(
                        encoded_state(&mut guest),
                        encoded_state(&mut unrestored),
                        "{case}"
                    );
                    assert_eq!(
                        access_counter(&mut guest, counter),
                        access_counter(&mut unrestored, counter),
                        "{case}"
                    );
                    assert_eq!(
                        encoded_state(&mut guest),
                        encoded_state(&mut unrestored),
                        "{case}"
                    );
                }
            }
        }
    }

    /// Every control word that a counter holds once programmed, with RW = 1,
    /// 2, or 3, any mode, including 6 and 7, which the 8254 takes as modes 2
    /// and 3, and either count format, survives a save and restore, after
    /// which the restored PIT reads and writes the counter as the saved one
    /// does.
    #[test]
    fn restore_accepts_every_programmable_control_word() {
        for counter in 0..3u8 {
            for rw in 1..=3u8 {
                for mode in 0..8u8 {
                    for bcd in 0..2u8 {
                        let control = (counter << 6) | (rw << 4) | (mode << 1) | bcd;
                        let mut saved = GuestPit::new();
                        saved.write(PIT_CONTROL_REGISTER, control);

                        let mut restored = GuestPit::new();
                        if let Err(err) = restored.pit.restore(saved.pit.save().unwrap()) {
                            panic!("control word {control:#04x}: {err:?}");
                        }
                        assert_eq!(
                            encoded_state(&mut restored),
                            encoded_state(&mut saved),
                            "control word {control:#04x}"
                        );
                        assert_eq!(
                            access_counter(&mut restored, counter.into()),
                            access_counter(&mut saved, counter.into()),
                            "control word {control:#04x}"
                        );
                    }
                }
            }
        }
    }
}
