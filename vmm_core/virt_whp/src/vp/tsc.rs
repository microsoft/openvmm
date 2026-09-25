// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RDTSC and TSC MSR exits of a restored SMP partition. While the partition's
//! restored-TSC clock (see [`crate::tsc`]) is active, guest timestamp reads
//! use its partition-wide TSC model.

use crate::WhpProcessor;
use crate::WhpResultExt;
use hvdef::Vtl;
use virt::VpHaltReason;
use virt::io::CpuIo;
use whp::set_registers;

impl WhpProcessor<'_> {
    /// Handles an RDTSC or RDTSCP exit.
    pub(super) fn handle_rdtsc_exit(
        &self,
        dev: &impl CpuIo,
        info: &whp::abi::WHV_X64_RDTSC_CONTEXT,
        exit: whp::Exit<'_>,
    ) -> Result<(), VpHaltReason> {
        self.handle_rdtsc(info, exit)
            .map_err(|error| dev.fatal_error(error.into()))
    }

    /// Handles a TSC, TSC_ADJUST, or TSC deadline MSR exit while the
    /// restored-TSC clock is active. Returns whether the access was handled.
    pub(super) fn handle_restored_tsc_msr_exit(
        &self,
        dev: &impl CpuIo,
        info: &whp::abi::WHV_X64_MSR_ACCESS_CONTEXT,
        exit: whp::Exit<'_>,
    ) -> Result<bool, VpHaltReason> {
        self.handle_restored_tsc_msr(info, exit)
            .map_err(|error| dev.fatal_error(error.into()))
    }

    fn inject_general_protection_fault(&self) -> Result<(), crate::Error> {
        let event = hvdef::HvX64PendingExceptionEvent::new()
            .with_event_pending(true)
            .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
            .with_deliver_error_code(true)
            .with_vector(0xd);
        self.current_whp()
            .set_register(whp::Register128::PendingEvent, event.into())
            .for_op("inject general protection fault")
    }

    fn read_restored_tsc(&self) -> Result<u64, crate::Error> {
        let reference = self
            .current_vtlp()
            .whp
            .reference_time()
            .for_op("read guest TSC reference time")?;
        let value = {
            let clock = self.vp.partition.clock.restored_tsc.lock();
            clock
                .as_ref()
                .ok_or(crate::Error::UnexpectedTimestampExit)?
                .read(self.vp.index.index(), reference)
        };
        match value {
            Some(value) => Ok(value),
            None => self
                .current_whp()
                .get_register(whp::Register64::Tsc)
                .for_op("read guest-programmed TSC"),
        }
    }

    fn complete_tsc_read(
        &self,
        value: u64,
        rip: u64,
        tsc_aux: Option<u64>,
    ) -> Result<(), crate::Error> {
        let rax = value & 0xffff_ffff;
        let rdx = value >> 32;
        match tsc_aux {
            Some(aux) => {
                set_registers!(
                    self.current_whp(),
                    [
                        (whp::Register64::Rax, rax),
                        (whp::Register64::Rdx, rdx),
                        (whp::Register64::Rip, rip),
                        (whp::Register64::Rcx, aux & 0xffff_ffff),
                    ],
                )
                .for_op("complete guest timestamp read")?;
            }
            None => {
                set_registers!(
                    self.current_whp(),
                    [
                        (whp::Register64::Rax, rax),
                        (whp::Register64::Rdx, rdx),
                        (whp::Register64::Rip, rip),
                    ],
                )
                .for_op("complete guest timestamp read")?;
            }
        }
        Ok(())
    }

    fn handle_rdtsc(
        &self,
        info: &whp::abi::WHV_X64_RDTSC_CONTEXT,
        exit: whp::Exit<'_>,
    ) -> Result<(), crate::Error> {
        if exit.vp_context.ExecutionState.Cpl() != 0 {
            let cr4 = self
                .current_whp()
                .get_register(whp::Register64::Cr4)
                .for_op("check timestamp access permission")?;
            if cr4 & x86defs::X64_CR4_TSD != 0 {
                return self.inject_general_protection_fault();
            }
        }
        let value = self.read_restored_tsc()?;
        if self.state.exits.rdtsc.get() < 4 {
            tracing::debug!(
                vp = self.vp.index.index(),
                value,
                whp_tsc = info.Tsc,
                whp_reference_time = info.ReferenceTime,
                "restored timestamp read"
            );
        }
        let rip = exit
            .vp_context
            .Rip
            .wrapping_add(u64::from(exit.vp_context.InstructionLength()));
        let tsc_aux = (info.RdtscInfo & 1 != 0).then_some(info.TscAux);
        self.complete_tsc_read(value, rip, tsc_aux)
    }

    fn handle_restored_tsc_msr(
        &self,
        info: &whp::abi::WHV_X64_MSR_ACCESS_CONTEXT,
        exit: whp::Exit<'_>,
    ) -> Result<bool, crate::Error> {
        if self.state.active_vtl != Vtl::Vtl0
            || !matches!(
                info.MsrNumber,
                x86defs::X86X_MSR_TSC
                    | crate::tsc::IA32_TSC_ADJUST
                    | x86defs::X86X_MSR_TSC_DEADLINE
            )
        {
            return Ok(false);
        }
        let tsc_adjust_supported = {
            let clock = self.vp.partition.clock.restored_tsc.lock();
            let Some(clock) = clock.as_ref() else {
                return Ok(false);
            };
            clock.tsc_adjust_supported
        };
        if exit.vp_context.ExecutionState.Cpl() != 0 {
            self.inject_general_protection_fault()?;
            return Ok(true);
        }
        let rcx = self
            .current_whp()
            .get_register(whp::Register64::Rcx)
            .for_op("read timestamp MSR selector")?;
        let msr = crate::tsc::msr_index(info.MsrNumber, info.AccessInfo.IsWrite(), rcx);
        if !matches!(
            msr,
            x86defs::X86X_MSR_TSC | crate::tsc::IA32_TSC_ADJUST | x86defs::X86X_MSR_TSC_DEADLINE
        ) || (msr == crate::tsc::IA32_TSC_ADJUST && !tsc_adjust_supported)
            || (msr == x86defs::X86X_MSR_TSC_DEADLINE && !self.vp.partition.caps.tsc_deadline)
        {
            self.inject_general_protection_fault()?;
            return Ok(true);
        }

        let rip = exit.vp_context.Rip.wrapping_add(2);
        if info.AccessInfo.IsWrite() {
            let value = (info.Rax & 0xffff_ffff) | (info.Rdx << 32);
            let changed = if msr == x86defs::X86X_MSR_TSC_DEADLINE {
                self.current_whp()
                    .set_register(whp::Register64::TscDeadline, value)
                    .for_op("write guest TSC deadline")?;
                false
            } else if msr == crate::tsc::IA32_TSC_ADJUST {
                let previous = self
                    .current_whp()
                    .get_register(whp::Register64::TscAdjust)
                    .for_op("read guest TSC adjustment")?;
                self.current_whp()
                    .set_register(whp::Register64::TscAdjust, value)
                    .for_op("write guest TSC adjustment")?;
                value != previous
            } else {
                self.current_whp()
                    .set_register(whp::Register64::Tsc, value)
                    .for_op("write guest TSC")?;
                true
            };
            if changed {
                self.vp
                    .partition
                    .clock
                    .restored_tsc
                    .lock()
                    .as_mut()
                    .ok_or(crate::Error::UnexpectedTimestampExit)?
                    .guest_write(self.vp.index.index());
            }
            self.current_whp()
                .set_register(whp::Register64::Rip, rip)
                .for_op("complete guest TSC write")?;
        } else {
            let value = if msr == x86defs::X86X_MSR_TSC_DEADLINE {
                self.current_whp()
                    .get_register(whp::Register64::TscDeadline)
                    .for_op("read guest TSC deadline")?
            } else if msr == crate::tsc::IA32_TSC_ADJUST {
                self.current_whp()
                    .get_register(whp::Register64::TscAdjust)
                    .for_op("read guest TSC adjustment")?
            } else {
                self.read_restored_tsc()?
            };
            self.complete_tsc_read(value, rip, None)?;
        }
        Ok(true)
    }
}
