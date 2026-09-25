// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Guest-visible RTC behavior modes.

use super::Rtc;
use super::RtcState;
use local_clock::InspectableLocalClock;
use vmcore::line_interrupt::LineInterrupt;
use vmcore::vmtime::VmTimeSource;

/// Guest-visible RTC behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcMode {
    /// Standard programmable MC146818-compatible behavior.
    Standard,
}

impl RtcState {
    /// Returns the initial register state for `mode`. All modes currently
    /// share the standard initial state.
    pub(super) fn with_mode(initial_cmos: Option<[u8; 256]>, _mode: RtcMode) -> Self {
        Self::new(initial_cmos)
    }
}

impl Rtc {
    /// Creates a CMOS RTC with an explicit guest-visible behavior mode.
    pub fn new_with_mode(
        real_time_source: Box<dyn InspectableLocalClock>,
        interrupt: LineInterrupt,
        vmtime_source: &VmTimeSource,
        century_reg_idx: u8,
        initial_cmos: Option<[u8; 256]>,
        enlightened_interrupts: bool,
        mode: RtcMode,
    ) -> Self {
        Rtc {
            mode,
            state: RtcState::with_mode(initial_cmos, mode),
            ..Self::new(
                real_time_source,
                interrupt,
                vmtime_source,
                century_reg_idx,
                initial_cmos,
                enlightened_interrupts,
            )
        }
    }
}
