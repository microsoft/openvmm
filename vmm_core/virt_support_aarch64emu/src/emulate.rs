// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrapper around aarch64emu for emulating single instructions to handle VM exits.

use crate::translate::TranslationRegisters;
use aarch64defs::EsrEl2;
use aarch64defs::FaultStatusCode;
use aarch64defs::IssInstructionAbort;
use aarch64emu::AccessCpuState;
use aarch64emu::InterceptState;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvAarch64PendingEvent;
use hvdef::HvAarch64PendingEventType;
use hvdef::HvInterceptAccessType;
use hvdef::HvMapGpaFlags;
use thiserror::Error;
use virt::EmulatorMonitorSupport;
use virt::VpHaltReason;
use virt::io::CpuIo;
use vm_topology::processor::VpIndex;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Support routines for the emulator.
pub trait EmulatorSupport: AccessCpuState {
    /// The current VP index.
    fn vp_index(&self) -> VpIndex;

    /// The physical address that caused the fault.
    fn physical_address(&self) -> Option<u64>;

    /// The gva translation included in the intercept message header, if valid.
    fn initial_gva_translation(&mut self) -> Option<InitialTranslation>;

    /// If interrupt pending is marked in the intercept message
    fn interruption_pending(&self) -> bool;

    /// Check that the current GPA is valid to access by the current VTL with the following access mode.
    /// Returns true if valid to access.
    fn check_vtl_access(
        &mut self,
        gpa: u64,
        mode: TranslateMode,
    ) -> Result<(), EmuCheckVtlAccessError>;

    /// Translates a GVA to a GPA.
    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError>;

    /// Generates an event (exception, guest nested page fault, etc.) in the guest.
    fn inject_pending_event(&mut self, event_info: HvAarch64PendingEvent);

    /// Get access to monitor support for the emulator, if it supports it.
    fn monitor_support(&self) -> Option<&dyn EmulatorMonitorSupport> {
        None
    }

    /// Returns true if `gpa` is mapped for the specified permissions.
    ///
    /// If true, then the emulator will use [`GuestMemory`] to access the GPA,
    /// and any failures will be fatal to the VM.
    ///
    /// If false, then the emulator will use [`CpuIo`] to access the GPA as
    /// MMIO.
    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool;
}

pub trait TranslateGvaSupport {
    type Error;

    /// Gets the object used to access the guest memory.
    fn guest_memory(&self) -> &GuestMemory;

    /// Acquires the TLB lock for this processor.
    fn acquire_tlb_lock(&mut self);

    /// Returns the registers used to walk the page table.
    fn registers(&mut self) -> Result<TranslationRegisters, Self::Error>;
}

/// The result of translate_gva on [`EmulatorSupport`].
pub struct EmuTranslateResult {
    /// The GPA result of the translation.
    pub gpa: u64,
    /// Whether the page is an overlay page.
    /// Not all implementations return overlay page or event_info yet, so these values are optional
    pub overlay_page: Option<bool>,
}

/// The translation, if any, provided in the intercept message and provided by [`EmulatorSupport`].
#[derive(Debug)]
pub struct InitialTranslation {
    /// GVA for the translation
    pub gva: u64,
    /// Translated gpa for the gva
    pub gpa: u64,
    // Whether the translation has read, write, or execute permissions.
    pub translate_mode: TranslateMode,
}

#[derive(Error, Debug)]
pub enum EmuCheckVtlAccessError {
    #[error("failed vtl permissions access for vtl {vtl:?} and access flags {denied_flags:?}")]
    AccessDenied {
        vtl: hvdef::Vtl,
        denied_flags: HvMapGpaFlags,
    },
}

#[derive(Error, Debug)]
#[error("translate gva to gpa returned non-successful code {code:?}")]
/// Error for a failed gva translation from [`EmulatorSupport`].
pub struct EmuTranslateError {
    /// Translate code of type hvdef::hypercall::TranslateGvaResultCode
    /// Should != Success
    pub code: hvdef::hypercall::TranslateGvaResultCode,
    /// Pending event, if any, returned by hypervisor to go with the translate code.
    pub event_info: Option<EsrEl2>,
}

/// The access type for a gva translation for [`EmulatorSupport`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TranslateMode {
    /// A read operation.
    Read,
    /// A write operation.
    Write,
    /// An execute operation.
    Execute,
}

/// The requested intercept access type isn't supported
#[derive(Debug)]
pub struct UnsupportedInterceptAccessType;

impl TryFrom<HvInterceptAccessType> for TranslateMode {
    type Error = UnsupportedInterceptAccessType;

    fn try_from(access_type: HvInterceptAccessType) -> Result<Self, Self::Error> {
        match access_type {
            HvInterceptAccessType::READ => Ok(TranslateMode::Read),
            HvInterceptAccessType::WRITE => Ok(TranslateMode::Write),
            HvInterceptAccessType::EXECUTE => Ok(TranslateMode::Execute),
            _ => Err(UnsupportedInterceptAccessType),
        }
    }
}

#[derive(Debug, Error)]
enum EmulationError {
    #[error("an interrupt caused the memory access exit")]
    InterruptionPending,
    #[error("emulator error (instruction {bytes:02x?})")]
    Emulator {
        bytes: Vec<u8>,
        #[source]
        error: aarch64emu::Error<Error>,
    },
}

/// Emulates an instruction.
pub async fn emulate<T: EmulatorSupport>(
    support: &mut T,
    intercept_state: &InterceptState,
    emu_mem: &GuestMemory,
    dev: &impl CpuIo,
) -> Result<(), VpHaltReason> {
    emulate_core(support, intercept_state, emu_mem, dev)
        .await
        .map_err(|e| dev.fatal_error(e.into()))
}

async fn emulate_core<T: EmulatorSupport>(
    support: &mut T,
    intercept_state: &InterceptState,
    gm: &GuestMemory,
    dev: &impl CpuIo,
) -> Result<(), EmulationError> {
    tracing::trace!(physical_address = support.physical_address(), "emulating");

    if support.interruption_pending() {
        // This means a fault or interruption *caused* the intercept
        // (and only really applies to memory intercept handling).
        // An example of how this could happen is if the
        // interrupt vector table itself is in mmio space; taking an
        // interrupt at that point requires that the processor reads the
        // vector out of the table, which generates an mmio intercept,
        // but not one associated with any particular instruction.
        // Therefore, there is nothing to emulate.
        //
        // A fault can't be injected into the guest because that could
        // cause an infinite loop (as the processor tries to get the trap
        // vector out of the mmio-ed vector table).  Just give up.

        return Err(EmulationError::InterruptionPending);
    }

    let mut cpu = EmulatorCpu::new(gm, dev, support, intercept_state.syndrome);
    let pc = cpu.pc();
    let result = {
        let mut emu = aarch64emu::Emulator::new(&mut cpu, intercept_state);
        emu.run().await
    };

    let instruction_bytes = if intercept_state.instruction_byte_count > 0 {
        intercept_state.instruction_bytes.to_vec()
    } else {
        vec![0, 0, 0, 0]
    };
    cpu.commit();

    if let Err(e) = result {
        match *e {
            aarch64emu::Error::MemoryAccess(addr, kind, err) => {
                if inject_memory_access_fault(addr, &err, support, intercept_state.syndrome) {
                    return Ok(());
                } else {
                    return Err(EmulationError::Emulator {
                        bytes: instruction_bytes,
                        error: aarch64emu::Error::MemoryAccess(addr, kind, err),
                    });
                };
            }
            err => {
                tracing::error!(
                    err = &err as &dyn std::error::Error,
                    len = instruction_bytes.len(),
                    physical_address = cpu.support.physical_address(),
                    "failed to emulate instruction"
                );
                let syndrome: EsrEl2 = IssInstructionAbort::new().into();
                cpu.support
                    .inject_pending_event(make_exception_event(syndrome, pc));
            }
        }
    }

    Ok(())
}

/// For storing gva to gpa translations in a cache in [`EmulatorCpu`]
struct GvaGpaCacheEntry {
    gva_page: u64,
    gpa_page: u64,
    translate_mode: TranslateMode,
}

impl GvaGpaCacheEntry {
    pub fn new(gva: u64, gpa: u64, translate_mode: TranslateMode) -> Self {
        GvaGpaCacheEntry {
            gva_page: gva >> hvdef::HV_PAGE_SHIFT,
            gpa_page: gpa >> hvdef::HV_PAGE_SHIFT,
            translate_mode,
        }
    }
}

struct EmulatorCpu<'a, T, U> {
    gm: &'a GuestMemory,
    support: &'a mut T,
    dev: &'a U,
    cached_translation: Option<GvaGpaCacheEntry>,
    syndrome: EsrEl2,
}

#[derive(Debug, Error)]
enum Error {
    #[error("translation error")]
    Translate(#[source] TranslateGvaError, Option<EsrEl2>),
    #[error("vtl permissions denied access for gpa {gpa}")]
    NoVtlAccess {
        gpa: u64,
        intercepting_vtl: hvdef::Vtl,
        denied_flags: HvMapGpaFlags,
    },
    #[error("failed to access mapped memory")]
    Memory(#[source] GuestMemoryError),
}

/// Result of a gva translation in [`EmulatorCpu`]
#[derive(Error, Debug)]
enum TranslateGvaError {
    #[error("gpa access denied code {0:?}")]
    AccessDenied(hvdef::hypercall::TranslateGvaResultCode),
    #[error("write on overlay page")]
    OverlayPageWrite,
    #[error("translation failed with unknown code {0:?}")]
    UnknownCode(hvdef::hypercall::TranslateGvaResultCode),
    #[error("translation failed with an intercept code")]
    Intercept,
    #[error("translation failed with a page fault-related code {0:?}")]
    PageFault(hvdef::hypercall::TranslateGvaResultCode),
}

impl<T: EmulatorSupport, U> EmulatorCpu<'_, T, U> {
    pub fn new<'a>(
        gm: &'a GuestMemory,
        dev: &'a U,
        support: &'a mut T,
        syndrome: EsrEl2,
    ) -> EmulatorCpu<'a, T, U> {
        let init_cache = {
            if let Some(InitialTranslation {
                gva,
                gpa,
                translate_mode,
            }) = support.initial_gva_translation()
            {
                tracing::trace!(
                    ?gva,
                    ?gpa,
                    ?translate_mode,
                    "adding initial translation to cache"
                );
                Some(GvaGpaCacheEntry::new(gva, gpa, translate_mode))
            } else {
                None
            }
        };

        EmulatorCpu {
            gm,
            dev,
            support,
            cached_translation: init_cache,
            syndrome,
        }
    }

    pub fn translate_gva(&mut self, gva: u64, mode: TranslateMode) -> Result<u64, Error> {
        type TranslateCode = hvdef::hypercall::TranslateGvaResultCode;

        // Note about invalid accesses at user mode: the exception code will
        // distinguish user vs kernel via _LOWER (e.g. kernel -> DATA_ABORT,
        // user -> DATA_ABORT_LOWER). We don't track that here though because
        // Hyper-V only takes the general version and will convert it depending
        // on the last execution state it has recorded.

        if let Some(GvaGpaCacheEntry {
            gva_page: cached_gva_page,
            gpa_page: cached_gpa_page,
            translate_mode: cached_mode,
        }) = self.cached_translation
        {
            if ((gva >> hvdef::HV_PAGE_SHIFT) == cached_gva_page) && (cached_mode == mode) {
                tracing::trace!(
                    ?gva,
                    ?cached_gva_page,
                    cached_gpa_page,
                    ?cached_mode,
                    "using cached entry"
                );
                return Ok((cached_gpa_page << hvdef::HV_PAGE_SHIFT) + (gva & (HV_PAGE_SIZE - 1)));
            }
        };

        match self.support.translate_gva(gva, mode) {
            Ok(EmuTranslateResult { gpa, overlay_page }) => {
                if overlay_page.is_some()
                    && overlay_page
                        .expect("should've already checked that the overlay page has value")
                    && (mode == TranslateMode::Write)
                {
                    // Parity: Reads of overlay pages are allowed for x64.
                    let mut syndrome: EsrEl2 = crate::translate::Error::GpaUnmapped(3).into();
                    syndrome.set_il(self.syndrome.il());
                    return Err(Error::Translate(TranslateGvaError::OverlayPageWrite, None));
                }

                let new_cache_entry = GvaGpaCacheEntry::new(gva, gpa, mode);

                self.cached_translation = Some(new_cache_entry);
                Ok(gpa)
            }
            Err(EmuTranslateError { code, event_info }) => match code {
                TranslateCode::INTERCEPT => {
                    tracing::trace!("translate gva to gpa returned an intercept event");
                    Err(Error::Translate(TranslateGvaError::Intercept, event_info))
                }
                TranslateCode::GPA_NO_READ_ACCESS
                | TranslateCode::GPA_NO_WRITE_ACCESS
                | TranslateCode::GPA_UNMAPPED
                | TranslateCode::GPA_ILLEGAL_OVERLAY_ACCESS
                | TranslateCode::GPA_UNACCEPTED => {
                    tracing::trace!("translate gva to gpa returned no access to page {:?}", code);
                    Err(Error::Translate(
                        TranslateGvaError::AccessDenied(code),
                        event_info,
                    ))
                }
                TranslateCode::PAGE_NOT_PRESENT
                | TranslateCode::PRIVILEGE_VIOLATION
                | TranslateCode::INVALID_PAGE_TABLE_FLAGS => {
                    tracing::trace!(gva, ?code, "translate gva to gpa returned");
                    Err(Error::Translate(
                        TranslateGvaError::PageFault(code),
                        event_info,
                    ))
                }
                TranslateCode::SUCCESS => unreachable!(),
                _ => {
                    tracing::trace!(
                        "translate error: unknown translation result code {:?}",
                        code
                    );

                    Err(Error::Translate(TranslateGvaError::UnknownCode(code), None))
                }
            },
        }
    }

    pub fn check_vtl_access(&mut self, gpa: u64, mode: TranslateMode) -> Result<(), Error> {
        self.support
            .check_vtl_access(gpa, mode)
            .map_err(|e| match e {
                EmuCheckVtlAccessError::AccessDenied { vtl, denied_flags } => Error::NoVtlAccess {
                    gpa,
                    intercepting_vtl: vtl,
                    denied_flags,
                },
            })
    }

    fn check_monitor_write(&self, gpa: u64, bytes: &[u8]) -> bool {
        if let Some(monitor_support) = self.support.monitor_support() {
            monitor_support.check_write(gpa, bytes)
        } else {
            false
        }
    }

    fn check_monitor_read(&self, gpa: u64, bytes: &mut [u8]) -> bool {
        if let Some(monitor_support) = self.support.monitor_support() {
            monitor_support.check_read(gpa, bytes)
        } else {
            false
        }
    }
}

impl<T: EmulatorSupport, U: CpuIo> aarch64emu::Cpu for EmulatorCpu<'_, T, U> {
    type Error = Error;

    async fn read_instruction(&mut self, gva: u64, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let gpa = match self.translate_gva(gva, TranslateMode::Execute) {
            Ok(g) => g,
            Err(e) => return Err(e),
        };
        self.read_physical_memory(gpa, bytes).await
    }

    async fn read_memory(&mut self, gva: u64, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let gpa = match self.translate_gva(gva, TranslateMode::Read) {
            Ok(g) => g,
            Err(e) => return Err(e),
        };
        self.read_physical_memory(gpa, bytes).await
    }

    async fn read_physical_memory(
        &mut self,
        gpa: u64,
        bytes: &mut [u8],
    ) -> Result<(), Self::Error> {
        self.check_vtl_access(gpa, TranslateMode::Read)?;

        if self.check_monitor_read(gpa, bytes) {
            Ok(())
        } else if self.support.is_gpa_mapped(gpa, false) {
            self.gm.read_at(gpa, bytes).map_err(Self::Error::Memory)
        } else {
            self.dev
                .read_mmio(self.support.vp_index(), gpa, bytes)
                .await;
            Ok(())
        }
    }

    async fn write_memory(&mut self, gva: u64, bytes: &[u8]) -> Result<(), Self::Error> {
        let gpa = match self.translate_gva(gva, TranslateMode::Write) {
            Ok(g) => g,
            Err(e) => return Err(e),
        };
        self.write_physical_memory(gpa, bytes).await
    }

    async fn write_physical_memory(&mut self, gpa: u64, bytes: &[u8]) -> Result<(), Self::Error> {
        self.check_vtl_access(gpa, TranslateMode::Write)?;

        if self.support.is_gpa_mapped(gpa, true) {
            self.gm.write_at(gpa, bytes).map_err(Self::Error::Memory)?;
        } else {
            self.dev
                .write_mmio(self.support.vp_index(), gpa, bytes)
                .await;
        }
        Ok(())
    }

    async fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        success: &mut bool,
    ) -> Result<(), Self::Error> {
        let gpa = match self.translate_gva(gva, TranslateMode::Write) {
            Ok(g) => g,
            Err(e) => return Err(e),
        };

        self.check_vtl_access(gpa, TranslateMode::Write)?;

        if self.check_monitor_write(gpa, new) {
            *success = true;
            Ok(())
        } else if self.support.is_gpa_mapped(gpa, true) {
            *success = match (current.len(), new.len()) {
                (1, 1) => self
                    .gm
                    .compare_exchange(gpa, current[0], new[0])
                    .map(|r| r.is_ok()),
                (2, 2) => self
                    .gm
                    .compare_exchange(
                        gpa,
                        u16::from_ne_bytes(current.try_into().unwrap()),
                        u16::from_ne_bytes(new.try_into().unwrap()),
                    )
                    .map(|r| r.is_ok()),
                (4, 4) => self
                    .gm
                    .compare_exchange(
                        gpa,
                        u32::from_ne_bytes(current.try_into().unwrap()),
                        u32::from_ne_bytes(new.try_into().unwrap()),
                    )
                    .map(|r| r.is_ok()),
                (8, 8) => self
                    .gm
                    .compare_exchange(
                        gpa,
                        u64::from_ne_bytes(current.try_into().unwrap()),
                        u64::from_ne_bytes(new.try_into().unwrap()),
                    )
                    .map(|r| r.is_ok()),
                _ => panic!("unsupported size for compare and write memory"),
            }
            .map_err(Self::Error::Memory)?;
            Ok(())
        } else {
            // Ignore the comparison aspect for device MMIO.
            *success = true;
            self.dev.write_mmio(self.support.vp_index(), gpa, new).await;
            Ok(())
        }
    }
}

impl<T: AccessCpuState, U: CpuIo> AccessCpuState for EmulatorCpu<'_, T, U> {
    fn commit(&mut self) {
        self.support.commit()
    }
    fn x(&mut self, index: u8) -> u64 {
        self.support.x(index)
    }
    fn update_x(&mut self, index: u8, data: u64) {
        self.support.update_x(index, data)
    }
    fn q(&self, index: u8) -> u128 {
        self.support.q(index)
    }
    fn update_q(&mut self, index: u8, data: u128) {
        self.support.update_q(index, data)
    }
    fn d(&self, index: u8) -> u64 {
        self.support.d(index)
    }
    fn update_d(&mut self, index: u8, data: u64) {
        self.support.update_d(index, data)
    }
    fn h(&self, index: u8) -> u32 {
        self.support.h(index)
    }
    fn update_h(&mut self, index: u8, data: u32) {
        self.support.update_h(index, data)
    }
    fn s(&self, index: u8) -> u16 {
        self.support.s(index)
    }
    fn update_s(&mut self, index: u8, data: u16) {
        self.support.update_s(index, data)
    }
    fn b(&self, index: u8) -> u8 {
        self.support.b(index)
    }
    fn update_b(&mut self, index: u8, data: u8) {
        self.support.update_b(index, data)
    }
    fn sp(&mut self) -> u64 {
        self.support.sp()
    }
    fn update_sp(&mut self, data: u64) {
        self.support.update_sp(data)
    }
    fn fp(&mut self) -> u64 {
        self.support.fp()
    }
    fn update_fp(&mut self, data: u64) {
        self.support.update_fp(data)
    }
    fn lr(&mut self) -> u64 {
        self.support.lr()
    }
    fn update_lr(&mut self, data: u64) {
        self.support.update_lr(data)
    }
    fn pc(&mut self) -> u64 {
        self.support.pc()
    }
    fn update_pc(&mut self, data: u64) {
        self.support.update_pc(data)
    }
    fn cpsr(&mut self) -> aarch64defs::Cpsr64 {
        self.support.cpsr()
    }
}

/// Creates a pending event for the exception type
pub fn make_exception_event(syndrome: EsrEl2, fault_address: u64) -> HvAarch64PendingEvent {
    let exception_event = hvdef::HvAarch64PendingExceptionEvent {
        header: hvdef::HvAarch64PendingEventHeader::new()
            .with_event_pending(true)
            .with_event_type(HvAarch64PendingEventType::EXCEPTION),
        syndrome: syndrome.into(),
        fault_address,
        _padding: Default::default(),
    };
    let exception_event_bytes = exception_event.as_bytes();
    let mut event = [0u8; 32];
    event.as_mut_slice()[..exception_event_bytes.len()].copy_from_slice(exception_event_bytes);
    HvAarch64PendingEvent::read_from_bytes(&event[..]).unwrap()
}

/// Injects an event into the guest if appropriate.
///
/// Returns true if an event was injected into the guest.
/// In the case of false being returned, the caller can
/// return the appropriate error code.
#[must_use]
fn inject_memory_access_fault<T: EmulatorSupport>(
    gva: u64,
    result: &Error,
    support: &mut T,
    syndrome: EsrEl2,
) -> bool {
    match result {
        Error::Translate(e, event) => {
            tracing::trace!(
                error = e as &dyn std::error::Error,
                "translation failed, injecting event"
            );

            if let Some(event_info) = event {
                support.inject_pending_event(make_exception_event(*event_info, gva));

                // The emulation did what it was supposed to do, which is throw a fault, so the emulation is done.
                return true;
            }
            false
        }
        Error::NoVtlAccess {
            gpa,
            intercepting_vtl: _,
            denied_flags,
        } => {
            tracing::trace!(
                error = result as &dyn std::error::Error,
                ?gva,
                ?gpa,
                "Vtl permissions checking failed"
            );

            let event = vtl_access_event(gva, *denied_flags, &syndrome);
            support.inject_pending_event(event);
            true
        }
        Error::Memory(_) => false,
    }
}

/// Generates the appropriate event for a VTL access error based
/// on the intercepting VTL
fn vtl_access_event(
    gva: u64,
    denied_access: HvMapGpaFlags,
    cur_syndrome: &EsrEl2,
) -> HvAarch64PendingEvent {
    assert!(denied_access.kernel_executable() || denied_access.user_executable());
    let inst_abort = IssInstructionAbort::new().with_ifsc(FaultStatusCode::PERMISSION_FAULT_LEVEL2);
    let mut syndrome: EsrEl2 = inst_abort.into();
    syndrome.set_il(cur_syndrome.il());
    make_exception_event(syndrome, gva)
}

/// Tries to emulate monitor page writes without taking the slower, full
/// emulation path.
///
/// The caller must have already validated that the fault was due to a write to
/// a monitor page GPA.
///
/// Returns the bit number being set within the monitor page.
pub fn emulate_mnf_write_fast_path<T: EmulatorSupport>(
    opcode: u32,
    support: &mut T,
    gm: &GuestMemory,
    dev: &impl CpuIo,
) -> Option<u64> {
    if support.interruption_pending() {
        return None;
    }

    // LDSETx / STSETx. A "fast path" is possible because we can assume the
    // MNF page is always zero-filled.
    if (opcode & 0x38203c00) == 0x38203000 {
        let mut cpu = EmulatorCpu::new(gm, dev, support, EsrEl2::from_bits(0));
        let size = (1 << (opcode >> 30)) * 8;
        let rs = (opcode >> 16) as u8 & 0x1f;
        let bitmask = if rs < 31 { cpu.x(rs) } else { 0 };
        let bitmask = if size == 64 {
            bitmask
        } else {
            bitmask & ((1 << size) - 1)
        };
        let rt = opcode as u8 & 0x1f;
        if rt != 31 {
            cpu.update_x(rt, 0);
        }

        let new_pc = cpu.pc().wrapping_add(4);
        cpu.update_pc(new_pc);
        cpu.commit();
        Some(bitmask)
    } else {
        tracelimit::warn_ratelimited!(
            opcode = format!("{:x}", opcode),
            "MNF fast path unknown opcode"
        );
        None
    }
}
