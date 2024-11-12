// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! CMOS RTC device (MC146818 compatible), as found on PC (and PC compatible)
//! platforms.

#![warn(missing_docs)]

use self::spec::CmosReg;
use self::spec::StatusRegA;
use self::spec::StatusRegB;
use self::spec::StatusRegC;
use self::spec::StatusRegD;
use self::spec::ENABLE_OSCILLATOR_CONTROL;
use chipset_device::io::IoError;
use chipset_device::io::IoResult;
use chipset_device::pio::PortIoIntercept;
use chipset_device::poll_device::PollDevice;
use chipset_device::ChipsetDevice;
use inspect::Inspect;
use inspect::InspectMut;
use local_clock::InspectableLocalClock;
use local_clock::LocalClockTime;
use std::ops::RangeInclusive;
use std::task::Poll;
use std::time::Duration;
use vmcore::device_state::ChangeDeviceState;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTime;
use vmcore::vmtime::VmTimeAccess;
use vmcore::vmtime::VmTimeSource;
use vmcore::vmtime::VmTimerPeriodic;

mod spec {
    //! Definitions pulled directly from the CMOS RTC spec sheet.

    use bitfield_struct::bitfield;
    use inspect::Inspect;

    open_enum::open_enum! {
        /// Standardized set of CMOS registers
        #[derive(Inspect)]
        #[inspect(debug)]
        pub enum CmosReg: u8 {
            SECOND       = 0x00,
            SECOND_ALARM = 0x01,
            MINUTE       = 0x02,
            MINUTE_ALARM = 0x03,
            HOUR         = 0x04,
            HOUR_ALARM   = 0x05,
            DAY_OF_WEEK  = 0x06,
            DAY_OF_MONTH = 0x07,
            MONTH        = 0x08,
            YEAR         = 0x09,
            STATUS_A     = 0x0A,
            STATUS_B     = 0x0B,
            STATUS_C     = 0x0C,
            STATUS_D     = 0x0D,
        }
    }

    impl CmosReg {
        /// Returns true if the register's value is tied to the real time.
        pub fn depends_on_rtc(&self, century: CmosReg) -> bool {
            matches!(
                *self,
                CmosReg::SECOND
                    | CmosReg::MINUTE
                    | CmosReg::HOUR
                    | CmosReg::DAY_OF_WEEK
                    | CmosReg::DAY_OF_MONTH
                    | CmosReg::MONTH
                    | CmosReg::YEAR
            ) || *self == century
        }

        /// Returns true if the register's value is tied to the Alarm
        pub fn depends_on_alarm(&self) -> bool {
            matches!(
                *self,
                CmosReg::SECOND_ALARM | CmosReg::MINUTE_ALARM | CmosReg::HOUR_ALARM
            )
        }
    }

    pub const ENABLE_OSCILLATOR_CONTROL: u8 = 0b010;

    /// Corresponding values for periodic_timer_rate values in range
    /// `0b0001` to `0b1111`
    pub const PERIODIC_TIMER_RATE_HZ: [usize; 15] = [
        256, 128, 8192, 4096, 2048, 1024, 512, 256, 128, 64, 32, 16, 8, 4, 2,
    ];

    #[rustfmt::skip]
    #[bitfield(u8)]
    pub struct StatusRegA {
        #[bits(4)] pub periodic_timer_rate: u8,
        #[bits(3)] pub oscillator_control: u8,
        #[bits(1)] pub update: bool,
    }

    #[rustfmt::skip]
    #[bitfield(u8)]
    pub struct StatusRegB {
        pub dst: bool, // not used in PCs
        pub h24_mode: bool,
        pub disable_bcd: bool,
        pub square_wave_enable: bool, // not used in PCs
        pub irq_enable_update: bool,
        pub irq_enable_alarm: bool,
        pub irq_enable_periodic: bool,
        pub set: bool,
    }

    #[rustfmt::skip]
    #[bitfield(u8)]
    pub struct StatusRegC {
        #[bits(4)] _unused: u8,
        pub irq_update: bool,
        pub irq_alarm: bool,
        pub irq_periodic: bool,
        pub irq_combined: bool,
    }

    #[rustfmt::skip]
    #[bitfield(u8)]
    pub struct StatusRegD {
        #[bits(7)] _unused: u8,
        /// Valid Ram And Time. Always set to 1 in emulated systems (as it's not
        /// like there's a real battery backing our rtc lol)
        pub vrt: bool,
    }

    pub const ALARM_WILDCARD: u8 = 0xFF;
}

open_enum::open_enum! {
    /// x86 standard RTC IO ports
    enum RtcIoPort: u16 {
        ADDR = 0x70,
        DATA = 0x71,
    }
}

/// Newtype around `[u8; 256]` that only supports indexing via [`CmosReg`]
#[derive(Debug, Clone, Inspect)]
#[inspect(transparent)]
struct CmosData(
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")] [u8; 256],
);

impl CmosData {
    fn empty() -> CmosData {
        CmosData([0; 256])
    }
}

impl std::ops::Index<CmosReg> for CmosData {
    type Output = u8;

    fn index(&self, index: CmosReg) -> &Self::Output {
        &self.0[index.0 as usize]
    }
}

impl std::ops::IndexMut<CmosReg> for CmosData {
    fn index_mut(&mut self, index: CmosReg) -> &mut Self::Output {
        &mut self.0[index.0 as usize]
    }
}

/// CMOS RTC device
#[derive(InspectMut)]
pub struct Rtc {
    // Static configuration
    century_reg: CmosReg,
    initial_cmos: Option<[u8; 256]>,
    enlightened_interrupts: bool,

    // Runtime deps
    real_time_source: Box<dyn InspectableLocalClock>,
    interrupt: LineInterrupt,
    vmtime_alarm: VmTimeAccess,
    vmtimer_periodic: VmTimerPeriodic,
    vmtimer_update: VmTimerPeriodic,

    // Runtime book-keeping
    #[inspect(debug)]
    last_update_bit_blip: LocalClockTime,

    // Volatile state
    state: RtcState,
}

#[derive(Debug, Inspect)]
struct RtcState {
    addr: u8,
    cmos: CmosData,
}

impl RtcState {
    fn new(initial_cmos: Option<[u8; 256]>) -> Self {
        let mut cmos = initial_cmos.map(CmosData).unwrap_or_else(CmosData::empty);

        cmos[CmosReg::STATUS_A] = {
            StatusRegA::new()
                .with_periodic_timer_rate(0b0110)
                .with_oscillator_control(ENABLE_OSCILLATOR_CONTROL)
                .into()
        };
        cmos[CmosReg::STATUS_B] = {
            StatusRegB::new()
                .with_disable_bcd(false)
                .with_h24_mode(true)
                .into()
        };
        cmos[CmosReg::STATUS_C] = StatusRegC::new().into();
        cmos[CmosReg::STATUS_D] = StatusRegD::new().with_vrt(true).into();

        Self {
            // technically, the default addr is undefined, but this is the
            // default Hyper-V used, so we'll stick with it
            addr: 0x80,
            cmos,
        }
    }
}

impl ChangeDeviceState for Rtc {
    fn start(&mut self) {}

    async fn stop(&mut self) {}

    async fn reset(&mut self) {
        self.state = RtcState::new(self.initial_cmos);

        self.update_timers();
        self.update_interrupt_line_level();
    }
}

impl ChipsetDevice for Rtc {
    fn supports_pio(&mut self) -> Option<&mut dyn PortIoIntercept> {
        Some(self)
    }

    fn supports_poll_device(&mut self) -> Option<&mut dyn PollDevice> {
        Some(self)
    }
}

fn to_bcd(n: u8) -> u8 {
    ((n / 10) << 4) | (n % 10)
}

fn from_bcd(n: u8) -> u8 {
    (n >> 4) * 10 + (n & 0xf)
}

/// Applies BCD + 24H time calculations on-top of HMS values fetched from CMOS.
fn canonical_hms(status_b: StatusRegB, mut hour: u8, mut min: u8, mut sec: u8) -> (u8, u8, u8) {
    if !status_b.disable_bcd() {
        sec = from_bcd(sec);
        min = from_bcd(min);

        if status_b.h24_mode() {
            hour = from_bcd(hour);
        } else {
            let hour_ampm = from_bcd(hour & 0x7F);
            if hour & 0x80 != 0 {
                hour = hour_ampm + 12;
            } else {
                hour = hour_ampm
            }
        }
    } else {
        if !status_b.h24_mode() {
            if hour & 0x80 != 0 {
                hour = (hour & 0x7F) + 12;
            }
        }
    }

    (hour, min, sec)
}

impl Rtc {
    /// Create a new CMOS RTC device
    ///
    /// `century_reg_idx` sets which byte of CMOS RAM to use as the century
    /// byte. This index is not standard between different platforms. e.g: on
    /// modern x86 platforms, the presence / index of the century register is
    /// determined by an entry in the FADT ACPI table.
    ///
    /// If `enlightened_interrupts`, then whenever a timer expires and the
    /// interrupt line would be set, pulse the interrupt line low then high to
    /// ensure an interrupt is delivered. This is used by older Windows guests
    /// to allow them to skip an extra PIO exit to clear the status C register.
    /// This behavior is indicated to the guest via an appropriate flag is set
    /// in the WAET ACPI table. See [Windows ACPI Emulated Devices Table][].
    ///
    /// [Windows ACPI Emulated Devices Table]:
    ///     <https://download.microsoft.com/download/7/E/7/7E7662CF-CBEA-470B-A97E-CE7CE0D98DC2/WAET.docx>
    pub fn new(
        real_time_source: Box<dyn InspectableLocalClock>,
        interrupt: LineInterrupt,
        vmtime_source: &VmTimeSource,
        century_reg_idx: u8,
        initial_cmos: Option<[u8; 256]>,
        enlightened_interrupts: bool,
    ) -> Self {
        Rtc {
            century_reg: CmosReg(century_reg_idx),
            initial_cmos,
            enlightened_interrupts,

            real_time_source,
            interrupt,
            vmtime_alarm: vmtime_source.access("rtc-alarm"),
            vmtimer_periodic: VmTimerPeriodic::new(vmtime_source.access("rtc-periodic")),
            vmtimer_update: VmTimerPeriodic::new(vmtime_source.access("rtc-update")),

            last_update_bit_blip: LocalClockTime::from_millis_since_unix_epoch(0),

            state: RtcState::new(initial_cmos),
        }
    }

    /// (for use by wrapper devices) reference to the raw underlying CMOS data
    pub fn raw_cmos(&mut self) -> &mut [u8; 256] {
        &mut self.state.cmos.0
    }

    /// Calculate the duration between the current time and the alarm time.
    ///
    /// (this logic was ported over from Hyper-V, instructive comment and all)
    ///
    /// * * *
    ///
    /// What an ugly mess.
    ///
    /// The alarm works like an alarm clock. So if you set it for 5:02:03, it
    /// will go off every day at 5:02:03. But if any of the time elements are
    /// 0xFF, that acts as a wildcard. So 5:FF:03 would go off at 5:00:03 and
    /// 5:01:03 and 5:02:03, etc. An alarm set for FF:FF:FF goes off every
    /// second.
    ///
    /// The problem is, what happens if, when we compute when the timer should
    /// go off, that it should go off NOW?
    ///
    /// Suppose, for instance, that they ask for 5:02:03, and at 5:02:03 we
    /// dutifully fire off the interrupt. The next thing we have to do is figure
    /// out when the alarm goes off next. It's supposed to go off next at
    /// 5:02:03, but if we've serviced the thing in reasonable time, it's
    /// *still* 5:02:03. We don't want to keep interrupting continuously for the
    /// next second, until it's 5:02:04. We want to wait a full day, and go off
    /// *tomorrow* at 5:02:03. Ahhh, but what if we're supposed to go off at
    /// 5:FF:03? In this case, we need to go off again in a minute. For 5:02:FF,
    /// it has to be in a second.
    ///
    /// Handling all of this is what the `go_around_again` variable is for.
    ///
    /// It will be set to the appropriate amount of time we need to wait if, by
    /// calculation, the alarm time would be immediate, as just described (which
    /// is the amount for the smallest unit of time which has a wildcard (24
    /// hours if none)).
    fn calculate_alarm_duration(&self) -> Duration {
        use self::spec::ALARM_WILDCARD;

        let status_b = StatusRegB::from(self.state.cmos[CmosReg::STATUS_B]);
        let (now_hour, now_min, now_sec) = canonical_hms(
            status_b,
            self.state.cmos[CmosReg::HOUR],
            self.state.cmos[CmosReg::MINUTE],
            self.state.cmos[CmosReg::SECOND],
        );
        let (alarm_hour, alarm_min, alarm_sec) = canonical_hms(
            status_b,
            self.state.cmos[CmosReg::HOUR_ALARM],
            self.state.cmos[CmosReg::MINUTE_ALARM],
            self.state.cmos[CmosReg::SECOND_ALARM],
        );

        let mut delta_hour: u8 = 0;
        let mut delta_min: u8 = 0;
        let mut delta_sec: u8 = 0;

        if alarm_sec == ALARM_WILDCARD {
            delta_sec = 0;
        } else {
            delta_sec = delta_sec.wrapping_add(alarm_sec.wrapping_sub(now_sec));
            if alarm_sec < now_sec {
                delta_sec = delta_sec.wrapping_add(60);
                delta_min = delta_min.wrapping_sub(1);
            }
        }

        if alarm_min == ALARM_WILDCARD {
            delta_min = 0;
        } else {
            delta_min = delta_min.wrapping_add(alarm_min.wrapping_sub(now_min));
            if alarm_min < now_min {
                delta_min = delta_min.wrapping_add(60);
                delta_hour = delta_hour.wrapping_sub(1);
            }
        }

        if alarm_hour == ALARM_WILDCARD {
            delta_hour = 0;
        } else {
            delta_hour = delta_hour.wrapping_add(alarm_hour.wrapping_sub(now_hour));
            if alarm_hour < now_hour {
                delta_hour = delta_hour.wrapping_add(24);
            }
        }

        const DURATION_SEC: Duration = Duration::from_secs(1);
        const DURATION_MIN: Duration = Duration::from_secs(60);
        const DURATION_HOUR: Duration = Duration::from_secs(60 * 60);
        const DURATION_DAY: Duration = Duration::from_secs(60 * 60 * 24);

        let go_around_again = match (alarm_sec, alarm_min, alarm_hour) {
            (ALARM_WILDCARD, _, _) => DURATION_SEC,
            (_, ALARM_WILDCARD, _) => DURATION_MIN,
            (_, _, ALARM_WILDCARD) => DURATION_HOUR,
            // if no wildcards were specified, then the alarm is set to fire in a day
            _ => DURATION_DAY,
        };

        let alarm_duration = {
            DURATION_HOUR * delta_hour as u32
                + DURATION_MIN * delta_min as u32
                + DURATION_SEC * delta_sec as u32
        };

        tracing::debug!(
            now = ?(now_hour, now_min, now_sec),
            alarm = ?(alarm_hour, alarm_min, alarm_sec),
            delta = ?(delta_hour, delta_min, delta_sec),
            ?go_around_again,
            ?alarm_duration,
            "setting alarm"
        );

        if alarm_duration.is_zero() {
            go_around_again
        } else {
            alarm_duration
        }
    }

    fn set_alarm_timer(&mut self, now: VmTime) {
        self.sync_clock_to_cmos();
        let alarm_duration = self.calculate_alarm_duration();

        self.vmtime_alarm
            .set_timeout(now.wrapping_add(alarm_duration));
    }

    fn on_alarm_timer(&mut self, now: VmTime) {
        let status_c = StatusRegC::from(self.state.cmos[CmosReg::STATUS_C]);
        self.state.cmos[CmosReg::STATUS_C] =
            status_c.with_irq_alarm(true).with_irq_combined(true).into();

        self.update_interrupt_line_level();

        // re-arm the alarm timer
        self.set_alarm_timer(now)
    }

    fn set_periodic_timer(&mut self) {
        use self::spec::PERIODIC_TIMER_RATE_HZ;

        let status_a = StatusRegA::from(self.state.cmos[CmosReg::STATUS_A]);

        // 0b0000 means the periodic timer is off
        if status_a.periodic_timer_rate() == 0 {
            return;
        }

        let tick_hz = PERIODIC_TIMER_RATE_HZ[status_a.periodic_timer_rate() as usize - 1];
        let tick_period = Duration::from_secs_f32(1. / tick_hz as f32);

        tracing::debug!(
            periodic_timer_rate = ?status_a.periodic_timer_rate(),
            ?tick_hz,
            ?tick_period,
            "setting periodic timer"
        );

        self.vmtimer_periodic.start(tick_period);
    }

    fn on_periodic_timer(&mut self) {
        let status_c = StatusRegC::from(self.state.cmos[CmosReg::STATUS_C]);
        self.state.cmos[CmosReg::STATUS_C] = status_c
            .with_irq_periodic(true)
            .with_irq_combined(true)
            .into();

        self.update_interrupt_line_level();
    }

    fn set_update_timer(&mut self) {
        self.vmtimer_update.start(Duration::from_secs(1));
    }

    fn on_update_timer(&mut self) {
        let status_c = StatusRegC::from(self.state.cmos[CmosReg::STATUS_C]);
        self.state.cmos[CmosReg::STATUS_C] = status_c
            .with_irq_update(true)
            .with_irq_combined(true)
            .into();

        self.update_interrupt_line_level();
    }

    /// Synchronizes the line level of the interrupt line with the state of
    /// status register C.
    fn update_interrupt_line_level(&self) {
        let status_c = StatusRegC::from(self.state.cmos[CmosReg::STATUS_C]);

        if status_c.irq_update() || status_c.irq_periodic() || status_c.irq_alarm() {
            assert!(status_c.irq_combined());
            if self.enlightened_interrupts {
                self.interrupt.set_level(false);
            }
            self.interrupt.set_level(true);
        } else {
            assert!(!status_c.irq_combined());
            self.interrupt.set_level(false);
        }
    }

    /// Synchronize vmtime-backed timers with current timer / alarm register
    /// configuration.
    fn update_timers(&mut self) {
        let status_b = StatusRegB::from(self.state.cmos[CmosReg::STATUS_B]);

        if status_b.irq_enable_alarm() {
            if self.vmtime_alarm.get_timeout().is_none() {
                self.set_alarm_timer(self.vmtime_alarm.now())
            }
        } else {
            self.vmtime_alarm.cancel_timeout();
        }

        if status_b.irq_enable_periodic() {
            if !self.vmtimer_periodic.is_running() {
                self.set_periodic_timer()
            }
        } else {
            self.vmtimer_periodic.cancel();
        }

        if status_b.irq_enable_update() {
            if !self.vmtimer_update.is_running() {
                self.set_update_timer()
            }
        } else {
            self.vmtimer_update.cancel();
        }
    }

    /// Directly write a byte in the CMOS.
    ///
    /// This method is marked `pub` to in order to implement wrapper devices
    /// that inject platform-specific CMOS memory contents. e.g: the AMI RTC
    /// device.
    pub fn set_cmos_byte(&mut self, addr: u8, data: u8) {
        let addr = CmosReg(addr);

        tracing::trace!(?addr, ?data, "set_cmos_byte");

        if (CmosReg::STATUS_A..=CmosReg::STATUS_D).contains(&addr) {
            self.set_status_byte(addr, data);
        } else {
            let old_data = self.state.cmos[addr];

            if data != old_data {
                // make sure the cmos time is up-to-date
                if addr.depends_on_rtc(self.century_reg) {
                    self.sync_clock_to_cmos();
                }

                self.state.cmos[addr] = data;

                // update the skew after setting the time
                if addr.depends_on_rtc(self.century_reg) {
                    self.sync_cmos_to_clock();
                }

                if addr.depends_on_alarm() || addr.depends_on_rtc(self.century_reg) {
                    if self.vmtime_alarm.get_timeout().is_some() {
                        self.set_alarm_timer(self.vmtime_alarm.now());
                    }
                }
            }
        }
    }

    /// Directly read a byte in the CMOS.
    ///
    /// This method is marked `pub` to in order to implement wrapper devices
    /// that inject platform-specific CMOS memory contents. e.g: the AMI RTC
    /// device.
    pub fn get_cmos_byte(&mut self, addr: u8) -> u8 {
        let addr = CmosReg(addr);

        let data = if (CmosReg::STATUS_A..=CmosReg::STATUS_D).contains(&addr) {
            self.get_status_byte(addr)
        } else {
            if addr.depends_on_rtc(self.century_reg) {
                self.sync_clock_to_cmos();
            }

            self.state.cmos[addr]
        };

        tracing::trace!(?addr, ?data, "get_cmos_byte");

        data
    }

    fn set_status_byte(&mut self, addr: CmosReg, data: u8) {
        match addr {
            CmosReg::STATUS_A => {
                let new_reg = StatusRegA::from(data);
                let old_reg = StatusRegA::from(self.state.cmos[CmosReg::STATUS_A]);

                // Determine if the oscillator is being programmed
                if new_reg.oscillator_control() != old_reg.oscillator_control() {
                    // need to re-prime alarm timer
                    self.vmtime_alarm.cancel_timeout();
                }

                if new_reg.periodic_timer_rate() != old_reg.periodic_timer_rate() {
                    // need to re-prime the periodic timer
                    self.vmtimer_periodic.cancel();
                }

                // update bit is read-only
                self.state.cmos[CmosReg::STATUS_A] = data & 0x7F;

                self.update_timers();
            }
            CmosReg::STATUS_B => {
                let mut new_reg = StatusRegB::from(data);

                // When updates are disabled, update interrupts are also disabled
                if new_reg.set() {
                    tracing::debug!("disable timer update and interrupt");
                    new_reg.set_irq_enable_update(false)
                }

                self.state.cmos[CmosReg::STATUS_B] = new_reg.into();

                self.update_timers();
            }
            // The AMI BIOS, in all its great wisdom, writes to these read-only registers.
            // We'll just silently allow that to happen...
            CmosReg::STATUS_C | CmosReg::STATUS_D => {}
            _ => unreachable!("passed invalid status reg"),
        }
    }

    fn get_status_byte(&mut self, addr: CmosReg) -> u8 {
        match addr {
            CmosReg::STATUS_A => {
                let mut data = StatusRegA::from(self.state.cmos[CmosReg::STATUS_A]);

                // The high bit of status A indicates a time update is in progress.
                // On real HW, this bit blips to 1 for a brief interval each second.
                // Guest OSes tend to use this brief blip once per second to calibrate
                // the rate of other timers such as the TSC.  Guest OSes tend to spin
                // wait for a rising or falling transition of the bit (typically rising edge),
                // and then the guest OS will wait for another of the same transition.
                if !StatusRegB::from(self.state.cmos[CmosReg::STATUS_B]).set() {
                    let now = self.real_time_source.get_time();
                    let elapsed = now - self.last_update_bit_blip;

                    // check if the programmed time jumped backwards
                    if elapsed.as_millis().is_negative() {
                        tracing::debug!("clock jumped backwards between update bit blips");
                        self.last_update_bit_blip = LocalClockTime::from_millis_since_unix_epoch(0);
                    }

                    tracing::trace!(
                        ?self.last_update_bit_blip,
                        ?now,
                        ?elapsed,
                        "get_status_byte"
                    );

                    let elapsed: time::Duration = elapsed.into();
                    if elapsed > time::Duration::seconds(1) {
                        // Update the date/time and note that we set the update bit now.
                        data.set_update(true);
                        self.sync_clock_to_cmos();
                        self.last_update_bit_blip = now;
                        tracing::trace!(
                            ?data,
                            ?elapsed,
                            cmos_date_time = ?self.read_cmos_date_time(),
                            "blip'd status a update bit"
                        );
                    }
                }

                data.into()
            }
            CmosReg::STATUS_B => self.state.cmos[CmosReg::STATUS_B],
            CmosReg::STATUS_C => {
                let data = StatusRegC::from(self.state.cmos[CmosReg::STATUS_C]);

                // clear pending interrupt flags.
                tracing::debug!("clearing rtc interrupts");
                self.state.cmos[CmosReg::STATUS_C] = StatusRegC::new().into();
                self.update_interrupt_line_level();

                data.into()
            }
            CmosReg::STATUS_D => {
                // always report valid ram time
                StatusRegD::new().with_vrt(true).into()
            }
            _ => unreachable!("passed invalid status reg"),
        }
    }

    fn read_cmos_date_time(&self) -> Result<time::PrimitiveDateTime, time::error::ComponentRange> {
        let mut sec = self.state.cmos[CmosReg::SECOND];
        let mut min = self.state.cmos[CmosReg::MINUTE];
        let mut hour = self.state.cmos[CmosReg::HOUR];
        let mut day = self.state.cmos[CmosReg::DAY_OF_MONTH];
        let mut month = self.state.cmos[CmosReg::MONTH];
        let mut year = self.state.cmos[CmosReg::YEAR];
        let mut century = self.state.cmos[self.century_reg];

        let status_b = StatusRegB::from(self.state.cmos[CmosReg::STATUS_B]);

        // factor in BCD, 24h time
        (hour, min, sec) = canonical_hms(status_b, hour, min, sec);
        if !status_b.disable_bcd() {
            (day, month, year, century) = (
                from_bcd(day),
                from_bcd(month),
                from_bcd(year),
                from_bcd(century),
            );
        }

        Ok(time::PrimitiveDateTime::new(
            time::Date::from_calendar_date(
                year as i32 + century as i32 * 100,
                month.try_into()?,
                day,
            )?,
            time::Time::from_hms(hour, min, sec)?,
        ))
    }

    /// Update the CMOS RTC registers with the current time from the backing
    /// `real_time_source`.
    fn sync_clock_to_cmos(&mut self) {
        if StatusRegA::from(self.state.cmos[CmosReg::STATUS_A]).oscillator_control()
            != ENABLE_OSCILLATOR_CONTROL
        {
            // Oscillator is disabled
            tracing::trace!(
                cmos_reg_status_a = self.state.cmos[CmosReg::STATUS_A],
                "sync_clock_to_cmos: Oscillator is disabled."
            );

            return;
        }

        let real_time = self.real_time_source.get_time();
        let Ok(clock_time): Result<time::OffsetDateTime, _> = real_time.try_into() else {
            tracelimit::warn_ratelimited!(
                ?real_time,
                "invalid date/time in real_time_source, skipping sync"
            );
            return;
        };

        let status_b = StatusRegB::from(self.state.cmos[CmosReg::STATUS_B]);

        self.state.cmos[CmosReg::SECOND] = clock_time.second();
        self.state.cmos[CmosReg::MINUTE] = clock_time.minute();
        self.state.cmos[CmosReg::HOUR] = {
            let hour = clock_time.hour();
            if status_b.h24_mode() {
                hour
            } else {
                if clock_time.hour() > 12 {
                    hour - 12
                } else {
                    hour
                }
            }
        };
        self.state.cmos[CmosReg::DAY_OF_WEEK] = clock_time.weekday().number_from_sunday();
        self.state.cmos[CmosReg::DAY_OF_MONTH] = clock_time.day();
        self.state.cmos[CmosReg::MONTH] = clock_time.month() as u8;
        self.state.cmos[CmosReg::YEAR] = (clock_time.year() % 100) as u8;
        self.state.cmos[self.century_reg] = (clock_time.year() / 100) as u8;

        if !status_b.disable_bcd() {
            let regs = [
                CmosReg::SECOND,
                CmosReg::MINUTE,
                CmosReg::HOUR,
                CmosReg::DAY_OF_WEEK,
                CmosReg::DAY_OF_MONTH,
                CmosReg::MONTH,
                CmosReg::YEAR,
                self.century_reg,
            ];

            for reg in regs {
                self.state.cmos[reg] = to_bcd(self.state.cmos[reg])
            }
        }

        if !status_b.h24_mode() {
            if clock_time.hour() > 12 {
                self.state.cmos[CmosReg::HOUR] |= 0x80;
            }
        }

        tracing::trace!(
            cmos_reg_status_b = self.state.cmos[CmosReg::STATUS_B],
            use_bcd_encoding = ?!status_b.disable_bcd(),
            use_24h_time = ?status_b.h24_mode(),
            cmos_date_time = ?self.read_cmos_date_time(),
            "sync_clock_to_cmos"
        );
    }

    /// Write-back the current contents of the CMOS RTC registers into the
    /// backing `real_time_source`
    fn sync_cmos_to_clock(&mut self) {
        let cmos_time: time::OffsetDateTime = match self.read_cmos_date_time() {
            Ok(cmos_time) => cmos_time.assume_utc(),
            Err(e) => {
                tracelimit::warn_ratelimited!(?e, "invalid date/time in RTC registers!");
                return;
            }
        };
        self.real_time_source.set_time(cmos_time.into());
    }

    #[cfg(test)]
    fn get_cmos_date_time(
        &mut self,
    ) -> Result<time::PrimitiveDateTime, time::error::ComponentRange> {
        self.sync_clock_to_cmos();
        self.read_cmos_date_time()
    }
}

impl PortIoIntercept for Rtc {
    fn io_read(&mut self, io_port: u16, data: &mut [u8]) -> IoResult {
        // We assume all accesses are one byte in size. Attempts to
        // access larger sizes will return a single byte of information
        // (zero-extended to the size of the access).
        data[0] = match RtcIoPort(io_port) {
            RtcIoPort::ADDR => self.state.addr,
            RtcIoPort::DATA => self.get_cmos_byte(self.state.addr),
            _ => return IoResult::Err(IoError::InvalidRegister),
        };

        IoResult::Ok
    }

    fn io_write(&mut self, io_port: u16, data: &[u8]) -> IoResult {
        // We assume all accesses are one byte in size. Attempts to
        // access larger sizes will return a single byte of information
        // (zero-extended to the size of the access).
        match RtcIoPort(io_port) {
            RtcIoPort::ADDR => {
                if data[0] & 0x7F != data[0] {
                    tracing::debug!("guest tried to set high-bit in CMOS addr register")
                }

                self.state.addr = data[0] & 0x7F
            }
            RtcIoPort::DATA => self.set_cmos_byte(self.state.addr, data[0]),
            _ => return IoResult::Err(IoError::InvalidRegister),
        }

        IoResult::Ok
    }

    fn get_static_regions(&mut self) -> &[(&str, RangeInclusive<u16>)] {
        &[("io", (RtcIoPort::ADDR.0)..=(RtcIoPort::DATA.0))]
    }
}

impl PollDevice for Rtc {
    fn poll_device(&mut self, cx: &mut std::task::Context<'_>) {
        while let Poll::Ready(now) = self.vmtime_alarm.poll_timeout(cx) {
            self.on_alarm_timer(now)
        }

        if let Poll::Ready(_now) = self.vmtimer_periodic.poll_timeout(cx) {
            self.on_periodic_timer()
        }

        if let Poll::Ready(_now) = self.vmtimer_update.poll_timeout(cx) {
            self.on_update_timer()
        }
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

        /// RTC saved state.
        #[derive(Protobuf, SavedStateRoot)]
        #[mesh(package = "chipset.cmos_rtc")]
        pub struct SavedState {
            #[mesh(1)]
            pub addr: u8,
            #[mesh(2)]
            pub cmos: [u8; 256],
        }
    }

    impl SaveRestore for Rtc {
        type SavedState = state::SavedState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let RtcState { addr, ref cmos } = self.state;

            let saved_state = state::SavedState { addr, cmos: cmos.0 };

            Ok(saved_state)
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            let state::SavedState { addr, cmos } = state;

            self.state = RtcState {
                addr,
                cmos: CmosData(cmos),
            };

            self.update_timers();
            self.update_interrupt_line_level();

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_clock::MockLocalClock;
    use local_clock::MockLocalClockAccessor;
    use test_with_tracing::test;

    fn new_test_rtc() -> (
        pal_async::DefaultPool,
        vmcore::vmtime::VmTimeKeeper,
        MockLocalClockAccessor,
        Rtc,
    ) {
        let mut pool = pal_async::DefaultPool::new();
        let driver = pool.driver();
        let vm_time_keeper =
            vmcore::vmtime::VmTimeKeeper::new(&pool.driver(), VmTime::from_100ns(0));
        let vm_time_source = pool
            .run_until(vm_time_keeper.builder().build(&driver))
            .unwrap();

        let time = MockLocalClock::new();
        let time_access = time.accessor();

        let rtc = Rtc::new(
            Box::new(time),
            LineInterrupt::detached(),
            &vm_time_source,
            0x32,
            None,
            false,
        );

        (pool, vm_time_keeper, time_access, rtc)
    }

    fn get_cmos_data(rtc: &mut Rtc, addr: CmosReg) -> u8 {
        let mut temp = [addr.0];
        rtc.io_write(RtcIoPort::ADDR.0, &temp).unwrap();
        rtc.io_read(RtcIoPort::DATA.0, &mut temp).unwrap();
        temp[0]
    }

    fn set_cmos_data(rtc: &mut Rtc, addr: CmosReg, data: u8) {
        let mut temp = [addr.0];
        rtc.io_write(RtcIoPort::ADDR.0, &temp).unwrap();
        temp[0] = data;
        rtc.io_write(RtcIoPort::DATA.0, &temp).unwrap();
    }

    fn get_rtc_data(rtc: &mut Rtc, addr: CmosReg, bcd: bool, hour24: bool) -> u8 {
        let mut temp = [addr.0];
        rtc.io_write(RtcIoPort::ADDR.0, &temp).unwrap();
        rtc.io_read(RtcIoPort::DATA.0, &mut temp).unwrap();
        let mut data = temp[0];
        if addr == CmosReg::HOUR {
            let pm = if hour24 { false } else { (data & 0x80) != 0 };
            if pm {
                data &= 0x7f
            };
            if bcd {
                data = from_bcd(data)
            };
            if pm {
                data += 12;
            }
        } else {
            if bcd {
                data = from_bcd(data)
            };
        }

        println!("get {0}({0:#x}) convert to {1}", temp[0], data);
        data
    }

    fn set_rtc_data(rtc: &mut Rtc, addr: CmosReg, data: u8, bcd: bool, hour24: bool) {
        let mut temp = [addr.0];
        rtc.io_write(RtcIoPort::ADDR.0, &temp).unwrap();
        let mut new_data = data;
        if addr == CmosReg::HOUR {
            let pm = if hour24 { false } else { new_data > 12 };
            if bcd {
                new_data =
                    to_bcd(if pm { new_data - 12 } else { new_data }) | if pm { 0x80 } else { 0 };
            } else {
                if pm {
                    new_data = (new_data - 12) | 0x80;
                }
            }
        } else {
            if bcd {
                new_data = to_bcd(new_data)
            };
        }

        println!("set {0} convert to {1}({1:#x})", data, new_data);
        temp[0] = new_data;
        rtc.io_write(RtcIoPort::DATA.0, &temp).unwrap();
    }

    // Local support routine.
    // Wait for CMOS update strobed rising edge.
    fn wait_for_edge(rtc: &mut Rtc, high: bool, time: MockLocalClockAccessor) -> bool {
        let limit = 5; //seconds
        let stall_ms = 10; //10ms to pause

        for _i in 0..(limit * 1000 / stall_ms) {
            let value = get_cmos_data(rtc, CmosReg::STATUS_A);
            if high {
                if value & 0x80 != 0 {
                    return true;
                }
            } else {
                if value & 0x80 == 0 {
                    return true;
                }
            }

            time.tick(Duration::from_millis(stall_ms));
        }

        false
    }

    fn set_bcd(rtc: &mut Rtc) {
        let mut value = get_cmos_data(rtc, CmosReg::STATUS_B);
        value &= 0xFB;
        set_cmos_data(rtc, CmosReg::STATUS_B, value);
    }

    fn set_binary(rtc: &mut Rtc) {
        let mut value = get_cmos_data(rtc, CmosReg::STATUS_B);
        value |= 0x4;
        set_cmos_data(rtc, CmosReg::STATUS_B, value);
    }

    fn set_24hour(rtc: &mut Rtc) {
        let mut value = get_cmos_data(rtc, CmosReg::STATUS_B);
        value |= 0x2;
        set_cmos_data(rtc, CmosReg::STATUS_B, value);
    }

    fn set_12hour(rtc: &mut Rtc) {
        let mut value = get_cmos_data(rtc, CmosReg::STATUS_B);
        value &= 0xFD;
        set_cmos_data(rtc, CmosReg::STATUS_B, value);
    }

    #[test]
    fn test_setup() {
        let (_, _, _, _rtc) = new_test_rtc();
    }

    #[test]
    fn test_default() {
        let default_state = RtcState::new(None);

        let (_, _, _, mut rtc) = new_test_rtc();

        let mut data = [0];
        rtc.io_read(RtcIoPort::ADDR.0, &mut data).unwrap();
        assert_eq!(data[0], 0x80);
        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_A),
            default_state.cmos[CmosReg::STATUS_A] | 0x80
        );
        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_B),
            default_state.cmos[CmosReg::STATUS_B]
        );
        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_C),
            default_state.cmos[CmosReg::STATUS_C]
        );
        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_D),
            default_state.cmos[CmosReg::STATUS_D]
        );
    }

    fn test_time_move(rtc: &mut Rtc, is_move: bool, time: MockLocalClockAccessor) {
        if let Ok(before) = rtc.get_cmos_date_time() {
            time.tick(Duration::from_secs(2));
            if let Ok(after) = rtc.get_cmos_date_time() {
                if is_move {
                    assert_ne!(before, after);
                } else {
                    assert_eq!(before, after);
                }
            } else {
                panic!("get_cmos_date_time failed");
            }
        } else {
            panic!("get_cmos_date_time failed");
        }
    }

    #[test]
    fn test_oscillator() {
        let default_state = RtcState::new(None);

        let (_, _, time, mut rtc) = new_test_rtc();

        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_A),
            default_state.cmos[CmosReg::STATUS_A] | 0x80
        );

        println!("RTC should move forward when oscillator is enabled (default control mask 010)");
        test_time_move(&mut rtc, true, time.clone());

        // Disable the oscillator
        set_cmos_data(&mut rtc, CmosReg::STATUS_A, 0x66);
        println!("RTC should not move forward when oscillator is disabled (control mask 110)");
        test_time_move(&mut rtc, false, time.clone());

        // Re-enable the oscillator
        set_cmos_data(&mut rtc, CmosReg::STATUS_A, 0x26);
        println!("RTC should move forward when oscillator is re-enabled (control mask 010)");
        test_time_move(&mut rtc, true, time);
    }

    #[test]
    fn test_uip() {
        let (_, _, time, mut rtc) = new_test_rtc();

        assert!(
            wait_for_edge(&mut rtc, false, time.clone())
                && wait_for_edge(&mut rtc, true, time.clone())
        );
        if let Ok(start) = rtc.get_cmos_date_time() {
            let seconds_to_wait: i64 = 10;
            for _i in 0..seconds_to_wait {
                assert!(
                    wait_for_edge(&mut rtc, false, time.clone())
                        && wait_for_edge(&mut rtc, true, time.clone())
                );
            }

            if let Ok(end) = rtc.get_cmos_date_time() {
                let elapsed = end - start;
                let expected = Duration::from_secs(seconds_to_wait as u64);
                let allowance = Duration::from_secs(1);
                println!("Expected: {:?} Start: {:?} End: {:?} Elapsed: {:?} Allowance: {:?}, RTC generates update strobe at expected rate.", expected, start, end, elapsed, allowance);
                assert!(elapsed <= (expected + allowance) && elapsed >= (expected - allowance));
            } else {
                panic!("get_cmos_date_time failed");
            }
        } else {
            panic!("get_cmos_date_time failed");
        }
    }

    #[test]
    fn test_readonly() {
        let default_state = RtcState::new(None);

        let (_, _, _, mut rtc) = new_test_rtc();

        assert_eq!(
            get_cmos_data(&mut rtc, CmosReg::STATUS_D),
            default_state.cmos[CmosReg::STATUS_D]
        );

        //Status D bits are read-only
        for i in 0..=0xFF {
            set_cmos_data(&mut rtc, CmosReg::STATUS_D, i);
            assert_eq!(
                get_cmos_data(&mut rtc, CmosReg::STATUS_D),
                default_state.cmos[CmosReg::STATUS_D]
            );
        }
    }

    #[test]
    fn test_writeable() {
        let (_, _, _, mut rtc) = new_test_rtc();

        //Registers 0x0f..0x7f should be writable, skip 0x32 which is century field of RTC
        for i in (0x0F..=0x7F).map(CmosReg) {
            if i == rtc.century_reg {
                continue;
            }

            set_cmos_data(&mut rtc, i, 0xFF);
            assert_eq!(get_cmos_data(&mut rtc, i), 0xFF);
            set_cmos_data(&mut rtc, i, 0);
            assert_eq!(get_cmos_data(&mut rtc, i), 0);
        }
    }

    fn test_count(bcd: bool, hour24: bool) {
        let (_, _, time, mut rtc) = new_test_rtc();

        if bcd {
            set_bcd(&mut rtc);
        } else {
            set_binary(&mut rtc);
        }

        if hour24 {
            set_24hour(&mut rtc);
        } else {
            set_12hour(&mut rtc);
        }

        let init: time::OffsetDateTime = time.get_time().try_into().unwrap();
        println!("init: {:?}", init);
        set_rtc_data(&mut rtc, CmosReg::HOUR, 11, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::MINUTE, 59, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::SECOND, 59, bcd, hour24);
        time.tick(Duration::from_secs(2));
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::SECOND, bcd, hour24), 1);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::MINUTE, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::HOUR, bcd, hour24), 12);
        assert_eq!(
            get_rtc_data(&mut rtc, CmosReg::DAY_OF_MONTH, bcd, hour24),
            init.day()
        );
        assert_eq!(
            get_rtc_data(&mut rtc, CmosReg::DAY_OF_WEEK, bcd, hour24),
            init.weekday().number_from_sunday()
        );

        set_rtc_data(&mut rtc, CmosReg::HOUR, 23, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::MINUTE, 59, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::SECOND, 59, bcd, hour24);
        time.tick(Duration::from_secs(2));
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::SECOND, bcd, hour24), 1);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::MINUTE, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::HOUR, bcd, hour24), 0);
        let temp = init + time::Duration::days(1);
        assert_eq!(
            get_rtc_data(&mut rtc, CmosReg::DAY_OF_MONTH, bcd, hour24),
            temp.day()
        );
        assert_eq!(
            get_rtc_data(&mut rtc, CmosReg::DAY_OF_WEEK, bcd, hour24),
            temp.weekday().number_from_sunday()
        );

        set_rtc_data(&mut rtc, CmosReg::MINUTE, 59, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::SECOND, 59, bcd, hour24);
        time.tick(Duration::from_secs(2));
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::SECOND, bcd, hour24), 1);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::MINUTE, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::HOUR, bcd, hour24), 1);
    }

    #[test]
    fn test_bcd_binary() {
        println!("----Testing BCD mode...");
        test_count(true, true);
        println!("----Testing Binary mode...");
        test_count(false, true);
    }

    #[test]
    fn test_hour_mode() {
        println!("----Testing Binary + 12 hour mode...");
        test_count(false, false);
        println!("----Testing Binary + 24 hour mode...");
        test_count(false, true);
        println!("----Testing BCD + 12 hour mode...");
        test_count(true, false);
        println!("----Testing BCD + 24 hour mode...");
        test_count(true, true);
    }

    fn test_day_of_month(bcd: bool, hour24: bool) {
        let century_reg_idx = 0x32;
        let century_reg = CmosReg(century_reg_idx);

        let (_, _, time, mut rtc) = new_test_rtc();

        if bcd {
            set_bcd(&mut rtc);
        } else {
            set_binary(&mut rtc);
        }

        if hour24 {
            set_24hour(&mut rtc);
        } else {
            set_12hour(&mut rtc);
        }

        for month in 1..=12 {
            let mut day;
            let mut year_check_count = 1;
            if month == 4 || month == 6 || month == 9 || month == 11 {
                day = 30;
            } else if month != 2 {
                day = 31;
            } else {
                day = 0;
                year_check_count = 4;
            }

            while year_check_count > 0 {
                let century = 30;
                let year = 4 + year_check_count;
                if month == 2 {
                    day = if (year & 3) > 0 { 28 } else { 29 };
                }

                println!(
                    "----Testing {:02}{:02}-{:02}-{:02}",
                    century, year, month, day
                );
                set_rtc_data(&mut rtc, century_reg, century, bcd, hour24);
                set_rtc_data(&mut rtc, CmosReg::YEAR, year, bcd, hour24);
                set_rtc_data(&mut rtc, CmosReg::MONTH, month, bcd, hour24);
                set_rtc_data(&mut rtc, CmosReg::DAY_OF_MONTH, day, bcd, hour24);
                set_rtc_data(&mut rtc, CmosReg::HOUR, 23, bcd, hour24);
                set_rtc_data(&mut rtc, CmosReg::MINUTE, 59, bcd, hour24);
                set_rtc_data(&mut rtc, CmosReg::SECOND, 59, bcd, hour24);
                time.tick(Duration::from_secs(2));
                assert_eq!(get_rtc_data(&mut rtc, CmosReg::SECOND, bcd, hour24), 1);
                assert_eq!(get_rtc_data(&mut rtc, CmosReg::MINUTE, bcd, hour24), 0);
                assert_eq!(get_rtc_data(&mut rtc, CmosReg::HOUR, bcd, hour24), 0);
                assert_eq!(
                    get_rtc_data(&mut rtc, CmosReg::DAY_OF_MONTH, bcd, hour24),
                    1
                );
                assert_eq!(
                    get_rtc_data(&mut rtc, CmosReg::MONTH, bcd, hour24),
                    if month == 12 { 1 } else { month + 1 }
                );
                assert_eq!(
                    get_rtc_data(&mut rtc, CmosReg::YEAR, bcd, hour24),
                    if month < 12 { year } else { year + 1 }
                );
                assert_eq!(get_rtc_data(&mut rtc, century_reg, bcd, hour24), century);
                year_check_count -= 1;
            }
        }
    }

    #[test]
    fn test_month() {
        println!("----Testing BCD mode...");
        test_day_of_month(true, true);
        println!("----Testing Binary mode...");
        test_day_of_month(false, true);
    }

    fn test_day_of_week(bcd: bool, hour24: bool) {
        let century_reg_idx = 0x32;
        let century_reg = CmosReg(century_reg_idx);

        let (_, _, time, mut rtc) = new_test_rtc();

        if bcd {
            set_bcd(&mut rtc);
        } else {
            set_binary(&mut rtc);
        }

        if hour24 {
            set_24hour(&mut rtc);
        } else {
            set_12hour(&mut rtc);
        }

        let century = 30;
        let year = 5;
        let month = 1;
        let day = 5;

        println!(
            "----Testing {:02}{:02}-{:02}-{:02}",
            century, year, month, day
        );
        set_rtc_data(&mut rtc, century_reg, century, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::YEAR, year, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::MONTH, month, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::DAY_OF_MONTH, day, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::DAY_OF_WEEK, 7, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::HOUR, 23, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::MINUTE, 59, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::SECOND, 59, bcd, hour24);
        time.tick(Duration::from_secs(2));
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::SECOND, bcd, hour24), 1);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::MINUTE, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::HOUR, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::DAY_OF_WEEK, bcd, hour24), 1);

        set_rtc_data(&mut rtc, CmosReg::DAY_OF_MONTH, day - 1, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::DAY_OF_WEEK, 6, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::HOUR, 23, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::MINUTE, 59, bcd, hour24);
        set_rtc_data(&mut rtc, CmosReg::SECOND, 59, bcd, hour24);
        time.tick(Duration::from_secs(2));
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::SECOND, bcd, hour24), 1);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::MINUTE, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::HOUR, bcd, hour24), 0);
        assert_eq!(get_rtc_data(&mut rtc, CmosReg::DAY_OF_WEEK, bcd, hour24), 7);
    }

    #[test]
    fn test_week() {
        println!("----Testing Binary mode...");
        test_day_of_week(false, true);
        println!("----Testing BCD mode...");
        test_day_of_week(true, true);
    }

    #[test]
    fn test_alarm() {
        fn hms_to_duration(h: u8, m: u8, s: u8) -> Duration {
            Duration::from_secs(s as u64 + 60 * m as u64 + h as u64 * 60 * 60)
        }

        let (_, _, _time, mut rtc) = new_test_rtc();

        set_binary(&mut rtc);

        set_cmos_data(&mut rtc, CmosReg::HOUR, 2);
        set_cmos_data(&mut rtc, CmosReg::MINUTE, 2);
        set_cmos_data(&mut rtc, CmosReg::SECOND, 2);

        set_cmos_data(&mut rtc, CmosReg::HOUR_ALARM, 3);
        set_cmos_data(&mut rtc, CmosReg::MINUTE_ALARM, 3);
        set_cmos_data(&mut rtc, CmosReg::SECOND_ALARM, 3);

        assert_eq!(rtc.calculate_alarm_duration(), hms_to_duration(1, 1, 1));
        set_cmos_data(&mut rtc, CmosReg::HOUR_ALARM, 0xff);
        set_cmos_data(&mut rtc, CmosReg::MINUTE_ALARM, 0xff);
        set_cmos_data(&mut rtc, CmosReg::SECOND_ALARM, 0xff);
        assert_eq!(rtc.calculate_alarm_duration(), hms_to_duration(0, 0, 1));

        // TODO: test some more alarm scenarios
    }
}
