// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Synthetic interrupt controller emulation.

use crate::RequestInterrupt;
use guestmem::GuestMemory;
use hvdef::HvAllArchRegisterName;
use hvdef::HvError;
use hvdef::HvMessage;
use hvdef::HvMessageHeader;
use hvdef::HvMessageType;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use hvdef::HvRegisterVsmVina;
use hvdef::HvResult;
use hvdef::HvSynicSimpSiefp;
use hvdef::HvSynicStimerConfig;
use hvdef::TimerMessagePayload;
use hvdef::HV_MESSAGE_SIZE;
use hvdef::HV_PAGE_SIZE;
use hvdef::HV_PAGE_SIZE_USIZE;
use hvdef::NUM_SINTS;
use inspect::Inspect;
use parking_lot::RwLock;
use std::array;
use std::sync::Arc;
use virt::x86::MsrError;
use vm_topology::processor::VpIndex;
use zerocopy::IntoBytes;

/// The virtual processor synthetic interrupt controller state.
#[derive(Inspect)]
pub struct ProcessorSynic {
    #[inspect(flatten)]
    sints: SintState,
    #[inspect(iter_by_index)]
    timers: [Timer; hvdef::NUM_TIMERS],
    #[inspect(skip)]
    shared: Arc<RwLock<SharedProcessorState>>,
    #[inspect(debug)]
    vina: HvRegisterVsmVina,
}

#[derive(Inspect)]
struct SintState {
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    siefp: HvSynicSimpSiefp,
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    simp: HvSynicSimpSiefp,
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    scontrol: hvdef::HvSynicScontrol,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(|x| inspect::AsHex(u64::from(*x)))")]
    sint: [hvdef::HvSynicSint; NUM_SINTS],
    pending_sints: u16,
    ready_sints: u16,
}

impl SintState {
    const AT_RESET: Self = Self {
        siefp: HvSynicSimpSiefp::new(),
        simp: HvSynicSimpSiefp::new(),
        scontrol: hvdef::HvSynicScontrol::new().with_enabled(true),
        sint: [hvdef::HvSynicSint::new().with_masked(true); NUM_SINTS],
        pending_sints: 0,
        ready_sints: 0,
    };
}

#[derive(Default, Inspect)]
struct Timer {
    #[inspect(with = "|x| inspect::AsHex(u64::from(*x))")]
    config: HvSynicStimerConfig,
    count: u64,
    reevaluate: bool,
    due_time: Option<u64>,
}

#[derive(Inspect, Default)]
struct SharedProcessorState {
    online: bool,
    enabled: bool,
    #[inspect(hex)]
    siefp: Option<u64>,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(|x| inspect::AsHex(u64::from(*x)))")]
    sint: [hvdef::HvSynicSint; NUM_SINTS],
}

impl SharedProcessorState {
    const AT_RESET: Self = Self {
        online: true,
        enabled: true,
        siefp: None,
        sint: [hvdef::HvSynicSint::new().with_masked(true); NUM_SINTS],
    };
}

/// A partition-wide synthetic interrupt controller.
#[derive(Inspect)]
pub struct GlobalSynic {
    #[inspect(iter_by_index)]
    vps: Vec<Arc<RwLock<SharedProcessorState>>>,
}

fn sint_interrupt(request: &mut dyn RequestInterrupt, sint: hvdef::HvSynicSint) {
    assert!(
        !sint.masked() && !sint.proxy(),
        "caller should have verified sint was ready"
    );
    if !sint.polling() {
        request.request_interrupt(sint.vector().into(), sint.auto_eoi());
    }
}

impl Timer {
    fn evaluate(
        &mut self,
        timer_index: u32,
        sints: &mut SintState,
        ref_time_now: u64,
        guest_memory: &GuestMemory,
        interrupt: &mut dyn RequestInterrupt,
    ) -> Option<u64> {
        if self.reevaluate {
            self.reevaluate = false;
            self.due_time = if self.config.enabled() && self.count != 0 {
                Some(if self.config.periodic() {
                    ref_time_now.wrapping_add(self.count)
                } else {
                    self.count
                })
            } else {
                None
            };
        }

        let due_time = self.due_time?;
        let delta = due_time.wrapping_sub(ref_time_now);
        if (delta as i64) > 0 {
            return Some(due_time);
        }

        if self.config.direct_mode() {
            interrupt.request_interrupt(self.config.apic_vector().into(), false);
        } else {
            let sint_index = self.config.sint();
            let sint = sints.sint[sint_index as usize];
            if sint.proxy()
                || sint.masked()
                || sints.pending_sints & (1 << sint_index) != 0
                || !sints.check_sint_ready(guest_memory, sint_index)
            {
                return None;
            }

            let payload = TimerMessagePayload {
                timer_index,
                reserved: 0,
                expiration_time: due_time,
                delivery_time: ref_time_now,
            };

            let message = HvMessage::new(
                HvMessageType::HvMessageTypeTimerExpired,
                0,
                payload.as_bytes(),
            );

            sints.post_message(guest_memory, sint_index, &message, &mut *interrupt);
        }

        // One-shot timers must be disabled upon expiration.
        if !self.config.periodic() {
            self.config.set_enabled(false);
        }

        self.due_time = if self.config.periodic() {
            Some(ref_time_now.wrapping_add(self.count))
        } else {
            None
        };

        self.due_time
    }
}

/// Error returned when trying to wait for readiness or signal events to a
/// proxied SINT.
pub struct SintProxied;

impl GlobalSynic {
    /// Returns a new instance of the synthetic interrupt controller.
    pub fn new(max_vp_count: u32) -> Self {
        Self {
            vps: (0..max_vp_count).map(|_| Default::default()).collect(),
        }
    }

    /// Signals an event to the specified virtual processor.
    ///
    /// `interrupt` is called with the target APIC vector while holding a lock
    /// preventing the synic state from changing.
    ///
    /// Returns `true` if the event flag is newly signaled.
    pub fn signal_event(
        &self,
        guest_memory: &GuestMemory,
        vp: VpIndex,
        sint_index: u8,
        flag: u16,
        interrupt: &mut dyn RequestInterrupt,
    ) -> Result<bool, SintProxied> {
        let Some(vp) = self.vps.get(vp.index() as usize) else {
            return Ok(false);
        };
        let vp = vp.read();
        let sint_index = sint_index as usize;
        let sint = vp.sint[sint_index];
        let flag = flag as usize;
        if sint.proxy() {
            return Err(SintProxied);
        }
        if !vp.enabled || sint.masked() {
            return Ok(false);
        }
        let Some(siefp) = vp.siefp else {
            return Ok(false);
        };
        let byte_gpa = siefp + (sint_index * (HV_PAGE_SIZE_USIZE / NUM_SINTS) + flag / 8) as u64;
        let mask = 1 << (flag % 8);
        // Guess zero for the current value of the byte to avoid an extra read
        // in the common case.
        let mut byte: u8 = 0;
        loop {
            match guest_memory.compare_exchange(byte_gpa, byte, byte | mask) {
                Ok(Ok(_)) => break,
                Ok(Err(b)) => byte = b,
                Err(err) => {
                    tracing::warn!(
                        gpa = byte_gpa,
                        error = &err as &dyn std::error::Error,
                        "failed to set siefp bit"
                    );
                    return Ok(false);
                }
            }
            if byte & mask != 0 {
                // Already signaled.
                return Ok(false);
            }
        }
        sint_interrupt(interrupt, sint);
        Ok(true)
    }

    /// Adds a virtual processor to the synthetic interrupt controller state.
    pub fn add_vp(&self, vp_index: VpIndex) -> ProcessorSynic {
        let shared = self.vps[vp_index.index() as usize].clone();
        let old_shared = std::mem::replace(&mut *shared.write(), SharedProcessorState::AT_RESET);
        assert!(!old_shared.online);

        ProcessorSynic {
            sints: SintState::AT_RESET,
            timers: array::from_fn(|_| Timer::default()),
            shared,
            vina: HvRegisterVsmVina::new(),
        }
    }
}

impl ProcessorSynic {
    /// Resets the synic state back to its initial state.
    pub fn reset(&mut self) {
        let Self {
            sints,
            timers,
            shared,
            vina,
        } = self;
        *sints = SintState::AT_RESET;
        *timers = array::from_fn(|_| Timer::default());
        *shared.write() = SharedProcessorState::AT_RESET;
        *vina = HvRegisterVsmVina::new();
    }

    /// Returns the event flags page register.
    pub fn siefp(&self) -> u64 {
        self.sints.siefp.into()
    }

    /// Returns the message page register.
    pub fn simp(&self) -> u64 {
        self.sints.simp.into()
    }

    /// Returns the `SCONTROL` register.
    pub fn scontrol(&self) -> u64 {
        self.sints.scontrol.into()
    }

    /// Returns the `SVERSION` register.
    pub fn sversion(&self) -> u64 {
        1
    }

    /// Returns the end-of-message register.
    pub fn eom(&self) -> u64 {
        0
    }

    /// Returns the specified `SINT` register.
    pub fn sint(&self, n: u8) -> u64 {
        self.sints.sint[n as usize].into()
    }

    /// Returns the set of SINTs that are proxied to the host.
    pub fn proxied_sints(&self) -> u16 {
        self.sints
            .sint
            .iter()
            .enumerate()
            .fold(0, |v, (i, s)| v | ((s.proxy() as u16) << i))
    }

    /// Returns the specified synthetic timer configuration register.
    pub fn stimer_config(&self, n: usize) -> u64 {
        self.timers[n].config.into()
    }

    /// Returns the specified synthetic timer count register.
    pub fn stimer_count(&self, n: usize) -> u64 {
        self.timers[n].count
    }

    /// Returns the value of the VINA register.
    pub fn vina(&self) -> HvRegisterVsmVina {
        self.vina
    }

    /// Sets the event flags page register.
    pub fn set_siefp(&mut self, guest_memory: &GuestMemory, v: u64) {
        let siefp = HvSynicSimpSiefp::from(v);
        tracing::debug!(?siefp, "setting siefp");
        if siefp.enabled()
            && (!self.sints.siefp.enabled() || siefp.base_gpn() != self.sints.siefp.base_gpn())
        {
            // As with the SIMP, clear the bits when the base GPN changes.
            if let Err(err) =
                guest_memory.fill_at(siefp.base_gpn() * HV_PAGE_SIZE, 0, HV_PAGE_SIZE_USIZE)
            {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "failed to clear siefp page"
                );
            }
        }
        self.sints.siefp = siefp;
        let mut shared = self.shared.write();
        shared.siefp = siefp.enabled().then_some(siefp.base_gpn() * HV_PAGE_SIZE);
    }

    /// Sets the message page register.
    pub fn set_simp(&mut self, guest_memory: &GuestMemory, v: u64) {
        let simp = HvSynicSimpSiefp::from(v);
        tracing::debug!(?simp, "setting simp");
        if simp.enabled()
            && (!self.sints.simp.enabled() || simp.base_gpn() != self.sints.simp.base_gpn())
        {
            // Clear the message queues when SIMP is enabled or the base GPN
            // changes. Technically to simulate an overlay we should swap the
            // page with a stashed value. However, guests do not seem to rely on
            // preserving the original page contents or SIMP contents when they
            // are moved, so don't bother.
            if let Err(err) =
                guest_memory.fill_at(simp.base_gpn() * HV_PAGE_SIZE, 0, HV_PAGE_SIZE_USIZE)
            {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "failed to clear simp page"
                );
            }
        }
        self.sints.simp = simp;
    }

    /// Sets the `SCONTROL` register.
    pub fn set_scontrol(&mut self, v: u64) {
        self.sints.scontrol = v.into();
        self.shared.write().enabled = self.sints.scontrol.enabled();
    }

    /// Performs an end-of-message operation.
    pub fn set_eom(&mut self, _v: u64) {
        // FUTURE: mark that the processor should scan message queues.
    }

    /// Sets the specified `SINT` register.
    pub fn set_sint(&mut self, n: usize, v: u64) {
        let sint = v.into();
        self.sints.sint[n] = sint;
        self.shared.write().sint[n] = sint;
    }

    /// Sets the specified synthetic timer configuration register.
    pub fn set_stimer_config(&mut self, n: usize, v: u64) {
        let config = v.into();
        self.timers[n].config = config;
        self.timers[n].reevaluate = true;
    }

    /// Sets the specified synthetic timer count register.
    pub fn set_stimer_count(&mut self, n: usize, v: u64) {
        self.timers[n].count = v;
        if self.timers[n].config.auto_enable() && self.timers[n].count != 0 {
            self.timers[n].config.set_enabled(true);
        }

        self.timers[n].reevaluate = true;
    }

    /// Requests a notification when any of the requested sints has a free
    /// message slot.
    ///
    /// The free sints will be returned by [`Self::scan`].
    pub fn request_sint_readiness(&mut self, sints: u16) {
        self.sints.pending_sints = sints;
    }

    /// Writes a message to the message page.
    ///
    /// Returns `Err(HvError::ObjectInUse)` if the message slot is full.
    pub fn post_message(
        &mut self,
        guest_memory: &GuestMemory,
        sint_index: u8,
        message: &HvMessage,
        interrupt: &mut dyn RequestInterrupt,
    ) -> Result<(), HvError> {
        let sint = &self.sints.sint[sint_index as usize];
        if sint.masked() || sint.proxy() {
            return Err(HvError::InvalidSynicState);
        }
        if self.sints.ready_sints & (1 << sint_index) == 0 {
            return Err(HvError::ObjectInUse);
        }

        self.sints
            .post_message(guest_memory, sint_index, message, interrupt);

        Ok(())
    }

    fn reg_to_msr(reg: HvRegisterName) -> HvResult<u32> {
        Ok(match HvAllArchRegisterName(reg.0) {
            HvAllArchRegisterName::Sint0
            | HvAllArchRegisterName::Sint1
            | HvAllArchRegisterName::Sint2
            | HvAllArchRegisterName::Sint3
            | HvAllArchRegisterName::Sint4
            | HvAllArchRegisterName::Sint5
            | HvAllArchRegisterName::Sint6
            | HvAllArchRegisterName::Sint7
            | HvAllArchRegisterName::Sint8
            | HvAllArchRegisterName::Sint9
            | HvAllArchRegisterName::Sint10
            | HvAllArchRegisterName::Sint11
            | HvAllArchRegisterName::Sint12
            | HvAllArchRegisterName::Sint13
            | HvAllArchRegisterName::Sint14
            | HvAllArchRegisterName::Sint15 => {
                hvdef::HV_X64_MSR_SINT0 + (reg.0 - HvAllArchRegisterName::Sint0.0)
            }
            HvAllArchRegisterName::Scontrol => hvdef::HV_X64_MSR_SCONTROL,
            HvAllArchRegisterName::Sversion => hvdef::HV_X64_MSR_SVERSION,
            HvAllArchRegisterName::Sifp => hvdef::HV_X64_MSR_SIEFP,
            HvAllArchRegisterName::Sipp => hvdef::HV_X64_MSR_SIMP,
            HvAllArchRegisterName::Eom => hvdef::HV_X64_MSR_EOM,
            HvAllArchRegisterName::Stimer0Config
            | HvAllArchRegisterName::Stimer0Count
            | HvAllArchRegisterName::Stimer1Config
            | HvAllArchRegisterName::Stimer1Count
            | HvAllArchRegisterName::Stimer2Config
            | HvAllArchRegisterName::Stimer2Count
            | HvAllArchRegisterName::Stimer3Config
            | HvAllArchRegisterName::Stimer3Count => {
                hvdef::HV_X64_MSR_STIMER0_CONFIG + (reg.0 - HvAllArchRegisterName::Stimer0Config.0)
            }
            _ => {
                tracelimit::error_ratelimited!(?reg, "unknown synic register");
                return Err(HvError::UnknownRegisterName);
            }
        })
    }

    fn msrerr_to_hverr(err: MsrError) -> HvError {
        match err {
            MsrError::Unknown => HvError::UnknownRegisterName,
            MsrError::InvalidAccess => HvError::InvalidParameter,
        }
    }

    /// Writes a synthetic interrupt controller register.
    pub fn write_reg(
        &mut self,
        guest_memory: &GuestMemory,
        reg: HvRegisterName,
        v: HvRegisterValue,
    ) -> HvResult<()> {
        match HvAllArchRegisterName(reg.0) {
            HvAllArchRegisterName::VsmVina => {
                let v = HvRegisterVsmVina::from(v.as_u64());
                if v.reserved() != 0 {
                    return Err(HvError::InvalidParameter);
                }
                self.vina = v;
            }
            _ => self
                .write_msr(guest_memory, Self::reg_to_msr(reg)?, v.as_u64())
                .map_err(Self::msrerr_to_hverr)?,
        }
        Ok(())
    }

    /// Writes an x64 MSR.
    pub fn write_msr(
        &mut self,
        guest_memory: &GuestMemory,
        msr: u32,
        v: u64,
    ) -> Result<(), MsrError> {
        match msr {
            msr @ hvdef::HV_X64_MSR_STIMER0_CONFIG..=hvdef::HV_X64_MSR_STIMER3_COUNT => {
                let offset = msr - hvdef::HV_X64_MSR_STIMER0_CONFIG;
                let timer = (offset >> 1) as _;
                let is_count = offset & 1 != 0;
                if is_count {
                    self.set_stimer_count(timer, v);
                } else {
                    self.set_stimer_config(timer, v);
                }
            }
            _ => self.write_nontimer_msr(guest_memory, msr, v)?,
        }
        Ok(())
    }

    /// Writes a non-synthetic-timer x64 MSR.
    pub fn write_nontimer_msr(
        &mut self,
        guest_memory: &GuestMemory,
        msr: u32,
        v: u64,
    ) -> Result<(), MsrError> {
        match msr {
            hvdef::HV_X64_MSR_SCONTROL => self.set_scontrol(v),
            hvdef::HV_X64_MSR_SVERSION => return Err(MsrError::InvalidAccess),
            hvdef::HV_X64_MSR_SIEFP => self.set_siefp(guest_memory, v),
            hvdef::HV_X64_MSR_SIMP => self.set_simp(guest_memory, v),
            hvdef::HV_X64_MSR_EOM => self.set_eom(v),
            msr @ hvdef::HV_X64_MSR_SINT0..=hvdef::HV_X64_MSR_SINT15 => {
                self.set_sint((msr - hvdef::HV_X64_MSR_SINT0) as usize, v)
            }
            _ => return Err(MsrError::Unknown),
        }
        Ok(())
    }

    /// Reads a synthetic interrupt controller register.
    pub fn read_reg(&self, reg: HvRegisterName) -> HvResult<HvRegisterValue> {
        let v = match HvAllArchRegisterName(reg.0) {
            HvAllArchRegisterName::VsmVina => self.vina.into_bits(),
            _ => self
                .read_msr(Self::reg_to_msr(reg)?)
                .map_err(Self::msrerr_to_hverr)?,
        };
        Ok(v.into())
    }

    /// Reads an x64 MSR.
    pub fn read_msr(&self, msr: u32) -> Result<u64, MsrError> {
        let value = match msr {
            msr @ hvdef::HV_X64_MSR_STIMER0_CONFIG..=hvdef::HV_X64_MSR_STIMER3_COUNT => {
                let offset = msr - hvdef::HV_X64_MSR_STIMER0_CONFIG;
                let timer = (offset >> 1) as _;
                let is_count = offset & 1 != 0;
                if is_count {
                    self.stimer_count(timer)
                } else {
                    self.stimer_config(timer)
                }
            }
            _ => self.read_nontimer_msr(msr)?,
        };
        Ok(value)
    }

    /// Reads a non-synthetic-timer x64 MSR.
    pub fn read_nontimer_msr(&self, msr: u32) -> Result<u64, MsrError> {
        let value = match msr {
            hvdef::HV_X64_MSR_SCONTROL => self.scontrol(),
            hvdef::HV_X64_MSR_SVERSION => self.sversion(),
            hvdef::HV_X64_MSR_SIEFP => self.siefp(),
            hvdef::HV_X64_MSR_SIMP => self.simp(),
            hvdef::HV_X64_MSR_EOM => self.eom(),
            msr @ hvdef::HV_X64_MSR_SINT0..=hvdef::HV_X64_MSR_SINT15 => {
                self.sint((msr - hvdef::HV_X64_MSR_SINT0) as u8)
            }
            _ => return Err(MsrError::Unknown),
        };
        Ok(value)
    }

    /// Scans for pending messages and timers.
    ///
    /// Calls `interrupt` with the APIC vector to signal, possibly multiple
    /// times for different SINTs.
    ///
    /// Returns SINTs that are now deliverable after calls to
    /// [`Self::request_sint_readiness`].
    #[must_use]
    pub fn scan(
        &mut self,
        ref_time_now: u64,
        guest_memory: &GuestMemory,
        interrupt: &mut dyn RequestInterrupt,
    ) -> (u16, Option<u64>) {
        // Evaluate `ready_sints` for each pending SINT.
        if self.sints.pending_sints != 0 {
            for sint in 0..NUM_SINTS as u8 {
                if self.sints.pending_sints & (1 << sint) != 0 {
                    self.sints.check_sint_ready(guest_memory, sint);
                }
            }
        }

        // Scan for due timers.
        let mut next_ref_time: Option<u64> = None;
        if self
            .timers
            .iter()
            .any(|t| t.reevaluate || t.due_time.is_some())
        {
            for (timer_index, timer) in self.timers.iter_mut().enumerate() {
                if let Some(next) = timer.evaluate(
                    timer_index as u32,
                    &mut self.sints,
                    ref_time_now,
                    guest_memory,
                    interrupt,
                ) {
                    match next_ref_time.as_mut() {
                        Some(v) => *v = (*v).min(next),
                        None => next_ref_time = Some(next),
                    }
                }
            }
        }

        let ready_pending_sints = self.sints.ready_sints & self.sints.pending_sints;
        self.sints.pending_sints &= !ready_pending_sints;
        (ready_pending_sints, next_ref_time)
    }
}

impl SintState {
    fn post_message(
        &mut self,
        guest_memory: &GuestMemory,
        sint_index: u8,
        message: &HvMessage,
        interrupt: &mut dyn RequestInterrupt,
    ) {
        let sint = self.sint[sint_index as usize];

        assert!(
            !sint.masked() && !sint.proxy() && self.ready_sints & (1 << sint_index) != 0,
            "caller should have verified sint was ready"
        );

        let gpa =
            self.simp.base_gpn() * HV_PAGE_SIZE + (sint_index as usize * HV_MESSAGE_SIZE) as u64;
        write_message_page(guest_memory, gpa, message.as_bytes());
        self.ready_sints &= !(1 << sint_index);
        sint_interrupt(interrupt, sint);
    }

    fn check_sint_ready(&mut self, guest_memory: &GuestMemory, sint: u8) -> bool {
        if self.ready_sints & (1 << sint) != 0 {
            return true;
        }
        let gpa = self.simp.base_gpn() * HV_PAGE_SIZE + (sint as usize * HV_MESSAGE_SIZE) as u64;
        let mut header = match guest_memory.read_plain::<HvMessageHeader>(gpa) {
            Ok(header) => header,
            Err(err) => {
                tracelimit::error_ratelimited!(
                    gpa,
                    error = &err as &dyn std::error::Error,
                    "failed to read from message page"
                );
                return false;
            }
        };
        if header.typ != HvMessageType::HvMessageTypeNone {
            // The slot is full. Mark the message pending so that the guest forces an EOM.
            if !header.flags.message_pending() {
                header.flags.set_message_pending(true);
                write_message_page(guest_memory, gpa, header.as_bytes());
            }
            return false;
        }
        self.ready_sints |= 1 << sint;
        true
    }
}

fn write_message_page(guest_memory: &GuestMemory, gpa: u64, data: &[u8]) -> bool {
    if let Err(err) = guest_memory.write_at(gpa, data) {
        tracelimit::error_ratelimited!(
            gpa,
            error = &err as &dyn std::error::Error,
            "failed to write to message page"
        );
        return false;
    }
    true
}
