// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrapper around aarch64emu for emulating single instructions to handle VM exits.

use crate::translate::TranslationRegisters;
use aarch64defs::EsrEl2;
use aarch64defs::FaultStatusCode;
use aarch64defs::IssDataAbort;
use aarch64defs::IssInstructionAbort;
use aarch64emu::AccessCpuState;
use aarch64emu::InterceptState;
use aarch64emu::ReadKind;
use cvm_tracing::CVM_ALLOWED;
use cvm_tracing::CVM_CONFIDENTIAL;
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
    /// System control for data alignment, stack alignment, and endianness.
    fn sctlr(&mut self) -> aarch64defs::SctlrEl1;

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
    /// Code-only backends cannot report the walk level; synthesized translation
    /// and permission faults use level 3.
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
    /// A read using EL0 permissions.
    ReadUnprivileged,
    /// A write using EL0 permissions.
    WriteUnprivileged,
}

impl TranslateMode {
    pub(crate) fn is_write(self) -> bool {
        matches!(self, Self::Write | Self::WriteUnprivileged)
    }
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
        .map_err(|e| {
            let pc = support.pc();
            let sp = support.sp();
            let cpsr = support.cpsr();
            let gpa = support.physical_address();
            let initial_translation = support.initial_gva_translation();
            let int_pend = support.interruption_pending();
            let gpa_mapped = gpa.map(|a| support.is_gpa_mapped(a, false));
            tracing::warn!(
                CVM_ALLOWED,
                pc,
                sp,
                ?cpsr,
                gpa,
                ?initial_translation,
                int_pend,
                gpa_mapped,
                "emulation failed"
            );
            let xs = (0..=30).map(|i| (i, support.x(i))).collect::<Vec<_>>();
            tracing::warn!(CVM_CONFIDENTIAL, ?xs, "emulation failed");
            dev.fatal_error(e.into())
        })
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

    let mut cpu = EmulatorCpu::new(gm, dev, support);
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
            aarch64emu::Error::InstructionException(mut syndrome, address) => {
                syndrome.set_il(true);
                support.inject_pending_event(make_exception_event(syndrome, address));
            }
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
                tracelimit::error_ratelimited!(
                    err = &err as &dyn std::error::Error,
                    len = instruction_bytes.len(),
                    physical_address = cpu.support.physical_address(),
                    "failed to emulate instruction"
                );
                let syndrome = EsrEl2::new()
                    .with_ec(aarch64defs::ExceptionClass::UNKNOWN.0)
                    .with_il(true);
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
}

#[derive(Debug, Error)]
enum Error {
    #[error("translation error")]
    Translate(
        u64,
        TranslateMode,
        #[source] TranslateGvaError,
        Option<EsrEl2>,
    ),
    #[error("vtl permissions denied access for gpa {gpa}")]
    NoVtlAccess {
        gva: u64,
        gpa: u64,
        intercepting_vtl: hvdef::Vtl,
        denied_flags: HvMapGpaFlags,
    },
    #[error("failed to access mapped memory")]
    Memory(#[source] GuestMemoryError),
    #[error("no virtual translation for physical access at {0:#x}")]
    MissingPhysicalTranslation(u64),
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
    pub fn new<'a>(gm: &'a GuestMemory, dev: &'a U, support: &'a mut T) -> EmulatorCpu<'a, T, U> {
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
                    && mode.is_write()
                {
                    // Parity: Reads of overlay pages are allowed for x64.
                    return Err(Error::Translate(
                        gva,
                        mode,
                        TranslateGvaError::OverlayPageWrite,
                        None,
                    ));
                }

                let new_cache_entry = GvaGpaCacheEntry::new(gva, gpa, mode);

                self.cached_translation = Some(new_cache_entry);
                Ok(gpa)
            }
            Err(EmuTranslateError { code, event_info }) => match code {
                TranslateCode::INTERCEPT => {
                    tracing::trace!("translate gva to gpa returned an intercept event");
                    Err(Error::Translate(
                        gva,
                        mode,
                        TranslateGvaError::Intercept,
                        event_info,
                    ))
                }
                TranslateCode::GPA_NO_READ_ACCESS
                | TranslateCode::GPA_NO_WRITE_ACCESS
                | TranslateCode::GPA_UNMAPPED
                | TranslateCode::GPA_ILLEGAL_OVERLAY_ACCESS
                | TranslateCode::GPA_UNACCEPTED => {
                    tracing::trace!("translate gva to gpa returned no access to page {:?}", code);
                    Err(Error::Translate(
                        gva,
                        mode,
                        TranslateGvaError::AccessDenied(code),
                        event_info,
                    ))
                }
                TranslateCode::PAGE_NOT_PRESENT
                | TranslateCode::PRIVILEGE_VIOLATION
                | TranslateCode::INVALID_PAGE_TABLE_FLAGS => {
                    tracing::trace!(gva, ?code, "translate gva to gpa returned");
                    Err(Error::Translate(
                        gva,
                        mode,
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

                    Err(Error::Translate(
                        gva,
                        mode,
                        TranslateGvaError::UnknownCode(code),
                        None,
                    ))
                }
            },
        }
    }

    pub fn check_vtl_access(
        &mut self,
        gva: u64,
        gpa: u64,
        mode: TranslateMode,
    ) -> Result<(), Error> {
        self.support
            .check_vtl_access(gpa, mode)
            .map_err(|e| match e {
                EmuCheckVtlAccessError::AccessDenied { vtl, denied_flags } => Error::NoVtlAccess {
                    gva,
                    gpa,
                    intercepting_vtl: vtl,
                    denied_flags,
                },
            })
    }

    fn translate_pages(
        &mut self,
        gva: u64,
        len: usize,
        mode: TranslateMode,
    ) -> Result<Vec<(u64, usize)>, Error> {
        let mut pages = Vec::new();
        let mut offset = 0;
        while offset < len {
            let address = gva.wrapping_add(offset as u64);
            let gpa = self.translate_gva(address, mode)?;
            self.check_vtl_access(address, gpa, mode)?;
            let count =
                (len - offset).min((HV_PAGE_SIZE - (address & (HV_PAGE_SIZE - 1))) as usize);
            pages.push((gpa, count));
            offset += count;
        }
        Ok(pages)
    }

    fn physical_gva(&self, gpa: u64) -> Result<u64, Error> {
        let Some(entry) = &self.cached_translation else {
            return Err(Error::MissingPhysicalTranslation(gpa));
        };
        if gpa >> hvdef::HV_PAGE_SHIFT != entry.gpa_page {
            return Err(Error::MissingPhysicalTranslation(gpa));
        }
        Ok((entry.gva_page << hvdef::HV_PAGE_SHIFT) | (gpa & (HV_PAGE_SIZE - 1)))
    }

    fn big_endian(&mut self) -> bool {
        let sctlr = self.support.sctlr();
        if self.support.cpsr().el() == 0 {
            sctlr.e0e()
        } else {
            sctlr.ee()
        }
    }
}

impl<T: EmulatorSupport, U: CpuIo> EmulatorCpu<'_, T, U> {
    async fn read_gpa(&mut self, gpa: u64, bytes: &mut [u8]) -> Result<(), Error> {
        if self
            .support
            .monitor_support()
            .is_some_and(|m| m.check_read(gpa, bytes))
        {
            return Ok(());
        }
        if self.support.is_gpa_mapped(gpa, false) {
            // Scalar reads must not tear; a bytewise copy would lose atomicity.
            match bytes.len() {
                1 => self
                    .gm
                    .read_plain::<u8>(gpa)
                    .map(|v| bytes.copy_from_slice(&v.to_le_bytes())),
                2 => self
                    .gm
                    .read_plain::<u16>(gpa)
                    .map(|v| bytes.copy_from_slice(&v.to_le_bytes())),
                4 => self
                    .gm
                    .read_plain::<u32>(gpa)
                    .map(|v| bytes.copy_from_slice(&v.to_le_bytes())),
                8 => self
                    .gm
                    .read_plain::<u64>(gpa)
                    .map(|v| bytes.copy_from_slice(&v.to_le_bytes())),
                _ => self.gm.read_at(gpa, bytes),
            }
            .map_err(Error::Memory)
        } else {
            self.dev
                .read_mmio(self.support.vp_index(), gpa, bytes)
                .await;
            Ok(())
        }
    }

    async fn write_gpa(&mut self, gpa: u64, bytes: &[u8]) -> Result<(), Error> {
        if self.support.is_gpa_mapped(gpa, true) {
            match bytes.len() {
                1 => self.gm.write_plain(gpa, &bytes[0]),
                2 => self
                    .gm
                    .write_plain(gpa, &u16::from_le_bytes(bytes.try_into().unwrap())),
                4 => self
                    .gm
                    .write_plain(gpa, &u32::from_le_bytes(bytes.try_into().unwrap())),
                8 => self
                    .gm
                    .write_plain(gpa, &u64::from_le_bytes(bytes.try_into().unwrap())),
                _ => self.gm.write_at(gpa, bytes),
            }
            .map_err(Error::Memory)
        } else {
            self.dev
                .write_mmio(self.support.vp_index(), gpa, bytes)
                .await;
            Ok(())
        }
    }
}

impl<T: EmulatorSupport, U: CpuIo> aarch64emu::Cpu for EmulatorCpu<'_, T, U> {
    type Error = Error;

    fn sctlr(&mut self) -> aarch64defs::SctlrEl1 {
        self.support.sctlr()
    }

    fn supports_syndrome_decode(&self) -> bool {
        self.cached_translation.is_some()
    }

    async fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        kind: ReadKind,
    ) -> Result<(), Error> {
        // Preflight the full access before any MMIO read can have side effects.
        // RMW reads require write access; unprivileged loads honor UAO.
        let mode = match kind {
            ReadKind::Data => TranslateMode::Read,
            ReadKind::Instruction => TranslateMode::Execute,
            ReadKind::ReadModifyWrite => TranslateMode::Write,
            ReadKind::Unprivileged if !self.support.cpsr().uao() => TranslateMode::ReadUnprivileged,
            ReadKind::Unprivileged => TranslateMode::Read,
        };
        let pages = self.translate_pages(gva, bytes.len(), mode)?;

        // Read the saved physical ranges, then apply guest data endianness.
        let mut offset = 0;
        for (gpa, count) in pages {
            self.read_gpa(gpa, &mut bytes[offset..offset + count])
                .await?;
            offset += count;
        }
        if kind != ReadKind::Instruction && self.big_endian() {
            bytes.reverse();
        }
        Ok(())
    }

    async fn read_physical_memory(
        &mut self,
        gpa: u64,
        bytes: &mut [u8],
        exec: bool,
    ) -> Result<(), Self::Error> {
        let gva = self.physical_gva(gpa)?;
        self.check_vtl_access(
            gva,
            gpa,
            if exec {
                TranslateMode::Execute
            } else {
                TranslateMode::Read
            },
        )?;

        self.read_gpa(gpa, bytes).await?;
        if !exec && self.big_endian() {
            bytes.reverse();
        }
        Ok(())
    }

    async fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
        unprivileged: bool,
    ) -> Result<(), Self::Error> {
        // Validate every page before any store or MMIO write can take effect.
        let mode = if unprivileged && !self.support.cpsr().uao() {
            TranslateMode::WriteUnprivileged
        } else {
            TranslateMode::Write
        };
        let pages = self.translate_pages(gva, bytes.len(), mode)?;

        // Prepare bytes in guest order and write using the saved translations.
        let mut reversed = Vec::new();
        let bytes = if self.big_endian() {
            reversed.extend_from_slice(bytes);
            reversed.reverse();
            &reversed
        } else {
            bytes
        };
        let mut offset = 0;
        for (gpa, count) in pages {
            self.write_gpa(gpa, &bytes[offset..offset + count]).await?;
            offset += count;
        }
        Ok(())
    }

    async fn write_physical_memory(&mut self, gpa: u64, bytes: &[u8]) -> Result<(), Self::Error> {
        let gva = self.physical_gva(gpa)?;
        self.check_vtl_access(gva, gpa, TranslateMode::Write)?;
        let mut reversed = bytes.to_vec();
        if self.big_endian() {
            reversed.reverse();
        }
        self.write_gpa(gpa, &reversed).await
    }

    async fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        success: &mut bool,
    ) -> Result<(), Self::Error> {
        // The core has checked natural alignment, so the operand fits one page.
        let gpa = self.translate_gva(gva, TranslateMode::Write)?;

        self.check_vtl_access(gva, gpa, TranslateMode::Write)?;
        let mut current_bytes = current.to_vec();
        let mut new_bytes = new.to_vec();
        if self.big_endian() {
            current_bytes.reverse();
            new_bytes.reverse();
        }
        let current = current_bytes.as_slice();
        let new = new_bytes.as_slice();

        // Monitor handling can consume the write; RAM otherwise needs an atomic
        // compare-exchange, while device MMIO ignores the comparison.
        if self
            .support
            .monitor_support()
            .is_some_and(|m| m.check_write(gpa, new))
        {
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
    _address: u64,
    result: &Error,
    support: &mut T,
    syndrome: EsrEl2,
) -> bool {
    match result {
        Error::Translate(gva, mode, e, event) => {
            tracing::trace!(
                error = e as &dyn std::error::Error,
                "translation failed, injecting event"
            );

            let event = event.or_else(|| {
                let status = match e {
                    TranslateGvaError::PageFault(
                        hvdef::hypercall::TranslateGvaResultCode::PAGE_NOT_PRESENT,
                    ) => FaultStatusCode::TRANSLATION_FAULT_LEVEL3,
                    TranslateGvaError::PageFault(
                        hvdef::hypercall::TranslateGvaResultCode::PRIVILEGE_VIOLATION,
                    ) => FaultStatusCode::PERMISSION_FAULT_LEVEL3,
                    TranslateGvaError::PageFault(_) => FaultStatusCode::TRANSLATION_FAULT_LEVEL3,
                    TranslateGvaError::AccessDenied(_) | TranslateGvaError::OverlayPageWrite => {
                        FaultStatusCode::SYNCHRONOUS_EXTERNAL_ABORT
                    }
                    _ => return None,
                };
                Some(if *mode == TranslateMode::Execute {
                    IssInstructionAbort::new().with_ifsc(status).into()
                } else {
                    IssDataAbort::new()
                        .with_dfsc(status)
                        .with_wnr(mode.is_write())
                        .into()
                })
            });
            if let Some(mut event_info) = event {
                event_info.set_il(true);
                support.inject_pending_event(make_exception_event(event_info, *gva));

                // The emulation did what it was supposed to do, which is throw a fault, so the emulation is done.
                return true;
            }
            false
        }
        Error::NoVtlAccess {
            gva,
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

            let event = vtl_access_event(*gva, *denied_flags, &syndrome);
            support.inject_pending_event(event);
            true
        }
        Error::Memory(_) | Error::MissingPhysicalTranslation(_) => false,
    }
}

/// Generates the appropriate event for a VTL access error based
/// on the intercepting VTL
fn vtl_access_event(
    gva: u64,
    denied_access: HvMapGpaFlags,
    cur_syndrome: &EsrEl2,
) -> HvAarch64PendingEvent {
    let mut syndrome: EsrEl2 =
        if denied_access.kernel_executable() || denied_access.user_executable() {
            IssInstructionAbort::new()
                .with_ifsc(FaultStatusCode::PERMISSION_FAULT_LEVEL2)
                .into()
        } else {
            IssDataAbort::new()
                .with_dfsc(FaultStatusCode::PERMISSION_FAULT_LEVEL2)
                .with_wnr(denied_access.writable())
                .into()
        };
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
    if (opcode & 0x3f20fc00) == 0x38203000 {
        let mut cpu = EmulatorCpu::new(gm, dev, support);
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

#[cfg(test)]
mod tests {
    use super::*;
    use aarch64emu::Cpu;
    use pal_async::async_test;
    use test_with_tracing::test;

    struct Support {
        x: [u64; 31],
        q: [u128; 32],
        pc: u64,
        sp: u64,
        cpsr: aarch64defs::Cpsr64,
        sctlr: aarch64defs::SctlrEl1,
        fault: Option<u64>,
        denied: Option<(u64, TranslateMode)>,
        initial: Option<InitialTranslation>,
        mapped: bool,
        events: Vec<HvAarch64PendingEvent>,
        translations: Vec<(u64, TranslateMode)>,
    }

    impl Default for Support {
        fn default() -> Self {
            Self {
                x: [0; 31],
                q: [0; 32],
                pc: 0,
                sp: 0,
                cpsr: aarch64defs::Cpsr64::new(),
                sctlr: aarch64defs::SctlrEl1::new(),
                fault: None,
                denied: None,
                initial: None,
                mapped: true,
                events: Vec::new(),
                translations: Vec::new(),
            }
        }
    }

    impl AccessCpuState for Support {
        fn commit(&mut self) {}
        fn x(&mut self, i: u8) -> u64 {
            self.x[i as usize]
        }
        fn update_x(&mut self, i: u8, v: u64) {
            self.x[i as usize] = v;
        }
        fn q(&self, i: u8) -> u128 {
            self.q[i as usize]
        }
        fn update_q(&mut self, i: u8, v: u128) {
            self.q[i as usize] = v;
        }
        fn d(&self, i: u8) -> u64 {
            self.q(i) as u64
        }
        fn update_d(&mut self, i: u8, v: u64) {
            self.update_q(i, v as u128);
        }
        fn h(&self, i: u8) -> u32 {
            self.q(i) as u32
        }
        fn update_h(&mut self, i: u8, v: u32) {
            self.update_q(i, v as u128);
        }
        fn s(&self, i: u8) -> u16 {
            self.q(i) as u16
        }
        fn update_s(&mut self, i: u8, v: u16) {
            self.update_q(i, v as u128);
        }
        fn b(&self, i: u8) -> u8 {
            self.q(i) as u8
        }
        fn update_b(&mut self, i: u8, v: u8) {
            self.update_q(i, v as u128);
        }
        fn sp(&mut self) -> u64 {
            self.sp
        }
        fn update_sp(&mut self, v: u64) {
            self.sp = v;
        }
        fn fp(&mut self) -> u64 {
            self.x[29]
        }
        fn update_fp(&mut self, v: u64) {
            self.x[29] = v;
        }
        fn lr(&mut self) -> u64 {
            self.x[30]
        }
        fn update_lr(&mut self, v: u64) {
            self.x[30] = v;
        }
        fn pc(&mut self) -> u64 {
            self.pc
        }
        fn update_pc(&mut self, v: u64) {
            self.pc = v;
        }
        fn cpsr(&mut self) -> aarch64defs::Cpsr64 {
            self.cpsr
        }
    }

    impl EmulatorSupport for Support {
        fn sctlr(&mut self) -> aarch64defs::SctlrEl1 {
            self.sctlr
        }
        fn vp_index(&self) -> VpIndex {
            VpIndex::BSP
        }
        fn physical_address(&self) -> Option<u64> {
            None
        }
        fn initial_gva_translation(&mut self) -> Option<InitialTranslation> {
            self.initial.take()
        }
        fn interruption_pending(&self) -> bool {
            false
        }
        fn check_vtl_access(
            &mut self,
            gpa: u64,
            mode: TranslateMode,
        ) -> Result<(), EmuCheckVtlAccessError> {
            if self.denied == Some((gpa & !0xfff, mode)) {
                Err(EmuCheckVtlAccessError::AccessDenied {
                    vtl: hvdef::Vtl::Vtl1,
                    denied_flags: HvMapGpaFlags::new()
                        .with_readable(mode == TranslateMode::Read)
                        .with_writable(mode.is_write())
                        .with_kernel_executable(mode == TranslateMode::Execute),
                })
            } else {
                Ok(())
            }
        }
        fn translate_gva(
            &mut self,
            gva: u64,
            mode: TranslateMode,
        ) -> Result<EmuTranslateResult, EmuTranslateError> {
            self.translations.push((gva, mode));
            if self.fault == Some(gva & !0xfff) {
                Err(EmuTranslateError {
                    code: hvdef::hypercall::TranslateGvaResultCode::PAGE_NOT_PRESENT,
                    event_info: None,
                })
            } else {
                let gpa = match gva & !0xfff {
                    0x4000 => gva & 0xfff,
                    0x5000 => 0x3000 | (gva & 0xfff),
                    _ => gva,
                };
                Ok(EmuTranslateResult {
                    gpa,
                    overlay_page: None,
                })
            }
        }
        fn inject_pending_event(&mut self, event: HvAarch64PendingEvent) {
            self.events.push(event);
        }
        fn is_gpa_mapped(&self, _gpa: u64, _write: bool) -> bool {
            self.mapped
        }
    }

    struct Io;
    impl CpuIo for Io {
        fn is_mmio(&self, _address: u64) -> bool {
            true
        }
        fn acknowledge_pic_interrupt(&self) -> Option<u8> {
            unreachable!()
        }
        fn handle_eoi(&self, _irq: u32) {
            unreachable!()
        }
        async fn read_mmio(&self, _vp: VpIndex, _address: u64, _data: &mut [u8]) {
            panic!("unexpected MMIO read");
        }
        async fn write_mmio(&self, _vp: VpIndex, _address: u64, _data: &[u8]) {
            panic!("unexpected MMIO write");
        }
        async fn read_io(&self, _vp: VpIndex, _port: u16, _data: &mut [u8]) {
            unreachable!()
        }
        async fn write_io(&self, _vp: VpIndex, _port: u16, _data: &[u8]) {
            unreachable!()
        }
        fn fatal_error(&self, error: Box<dyn std::error::Error + Send + Sync>) -> VpHaltReason {
            panic!("{error:#}");
        }
    }

    fn exception(support: &Support) -> hvdef::HvAarch64PendingExceptionEvent {
        assert_eq!(support.events.len(), 1);
        hvdef::HvAarch64PendingExceptionEvent::read_from_prefix(support.events[0].as_bytes())
            .unwrap()
            .0
    }

    #[async_test]
    async fn cross_page_reads_and_writes_follow_each_translation() {
        for split in 1..8 {
            let gm = GuestMemory::allocate(0x4000);
            gm.fill_at(0, 0xa5, 0x4000).unwrap();
            let mut support = Support::default();
            let value = 0x1234_5678_9abc_def0u64.to_le_bytes();
            let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
            cpu.write_memory(0x5000 - split, &value, false)
                .await
                .unwrap();
            let mut expected = vec![0xa5; 0x4000];
            expected[0x1000 - split as usize..0x1000].copy_from_slice(&value[..split as usize]);
            expected[0x3000..0x3008 - split as usize].copy_from_slice(&value[split as usize..]);
            let mut actual = vec![0; expected.len()];
            gm.read_at(0, &mut actual).unwrap();
            assert_eq!(actual, expected);
            let mut read = [0; 8];
            cpu.read_memory(0x5000 - split, &mut read, ReadKind::Data)
                .await
                .unwrap();
            assert_eq!(read, value);
        }
    }

    #[async_test]
    async fn second_page_faults_precede_ram_and_mmio_effects() {
        for mapped in [false, true] {
            for write in [false, true] {
                for vtl in [false, true] {
                    let gm = GuestMemory::allocate(0x4000);
                    gm.fill_at(0, 0xa5, 0x4000).unwrap();
                    let mut support = Support {
                        mapped,
                        ..Default::default()
                    };
                    if vtl {
                        support.denied = Some((
                            0x3000,
                            if write {
                                TranslateMode::Write
                            } else {
                                TranslateMode::Read
                            },
                        ));
                    } else {
                        support.fault = Some(0x5000);
                    }
                    let mut data = [0x55; 8];
                    let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
                    let result = if write {
                        cpu.write_memory(0x4ffd, &data, false).await
                    } else {
                        cpu.read_memory(0x4ffd, &mut data, ReadKind::Data).await
                    };
                    let error = result.unwrap_err();
                    assert!(inject_memory_access_fault(
                        0x4ffd,
                        &error,
                        &mut support,
                        EsrEl2::new().with_il(true)
                    ));
                    let event = exception(&support);
                    assert_eq!(event.fault_address, 0x5000);
                    let syndrome: EsrEl2 = event.syndrome.into();
                    assert_eq!(syndrome.ec(), aarch64defs::ExceptionClass::DATA_ABORT.0);
                    assert_eq!(syndrome.wnr(), write);
                    assert_eq!(
                        syndrome.lower_iss(),
                        if vtl {
                            FaultStatusCode::PERMISSION_FAULT_LEVEL2.0
                        } else {
                            FaultStatusCode::TRANSLATION_FAULT_LEVEL3.0
                        }
                    );
                    assert_eq!(data, [0x55; 8]);
                    let mut actual = vec![0; 0x4000];
                    gm.read_at(0, &mut actual).unwrap();
                    assert!(actual.iter().all(|&b| b == 0xa5));
                }
            }
        }
    }

    #[async_test]
    async fn execute_faults_are_instruction_aborts() {
        let gm = GuestMemory::allocate(0x4000);
        let mut support = Support {
            fault: Some(0x5000),
            ..Default::default()
        };
        let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
        let error = cpu
            .read_memory(0x4ffe, &mut [0; 4], ReadKind::Instruction)
            .await
            .unwrap_err();
        assert!(inject_memory_access_fault(
            0x4ffe,
            &error,
            &mut support,
            EsrEl2::new()
        ));
        let event = exception(&support);
        let syndrome: EsrEl2 = event.syndrome.into();
        assert_eq!(
            syndrome.ec(),
            aarch64defs::ExceptionClass::INSTRUCTION_ABORT.0
        );
        assert_eq!(event.fault_address, 0x5000);
    }

    #[async_test]
    async fn data_endianness_and_atomic_permissions() {
        let gm = GuestMemory::allocate(0x1000);
        gm.write_at(0x100, &[0x12, 0x34, 0x56, 0x78]).unwrap();
        let mut support = Support {
            sctlr: aarch64defs::SctlrEl1::new().with_e0e(true),
            ..Default::default()
        };
        let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
        let mut bytes = [0; 4];
        cpu.read_memory(0x100, &mut bytes, ReadKind::Data)
            .await
            .unwrap();
        assert_eq!(u32::from_le_bytes(bytes), 0x1234_5678);
        cpu.write_memory(0x104, &0x8765_4321u32.to_le_bytes(), false)
            .await
            .unwrap();
        gm.read_at(0x104, &mut bytes).unwrap();
        assert_eq!(bytes, [0x87, 0x65, 0x43, 0x21]);
        let mut success = false;
        cpu.compare_and_write_memory(
            0x100,
            &0x1234_5678u32.to_le_bytes(),
            &0x5566_7788u32.to_le_bytes(),
            &mut success,
        )
        .await
        .unwrap();
        assert!(success);
        gm.read_at(0x100, &mut bytes).unwrap();
        assert_eq!(bytes, [0x55, 0x66, 0x77, 0x88]);
        support.denied = Some((0, TranslateMode::Write));
        let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
        assert!(
            cpu.read_memory(0x100, &mut bytes, ReadKind::ReadModifyWrite)
                .await
                .is_err()
        );
    }

    #[async_test]
    async fn narrow_mapped_loads_do_not_tear_under_concurrent_writes() {
        for size in [2, 4] {
            let gm = GuestMemory::allocate(0x1000);
            let patterns = if size == 2 {
                [0x55aau32, 0xaa55]
            } else {
                [0x55aa_55aau32, 0xaa55_aa55]
            };
            if size == 2 {
                gm.write_plain(0x100, &(patterns[0] as u16)).unwrap();
            } else {
                gm.write_plain(0x100, &patterns[0]).unwrap();
            }
            let writer_memory = gm.clone();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let writer_barrier = barrier.clone();
            let writer = std::thread::spawn(move || {
                writer_barrier.wait();
                for i in 0..100_000 {
                    if size == 2 {
                        writer_memory
                            .write_plain(0x100, &(patterns[i & 1] as u16))
                            .unwrap();
                    } else {
                        writer_memory.write_plain(0x100, &patterns[i & 1]).unwrap();
                    }
                }
            });
            let mut support = Support::default();
            let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
            barrier.wait();
            let mut torn = None;
            for _ in 0..4096 {
                let mut bytes = [0; 4];
                cpu.read_memory(0x100, &mut bytes[..size], ReadKind::Data)
                    .await
                    .unwrap();
                let value = u32::from_le_bytes(bytes);
                if !patterns.contains(&value) {
                    torn = Some(value);
                    break;
                }
            }
            writer.join().unwrap();
            assert_eq!(torn, None, "{size}-byte load tore");
        }
    }

    #[async_test]
    async fn unprivileged_translations_and_uao_are_distinct() {
        for uao in [false, true] {
            let gm = GuestMemory::allocate(0x1000);
            let mut support = Support::default();
            support.cpsr.set_el(1);
            support.cpsr.set_uao(uao);
            let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
            cpu.read_memory(0x100, &mut [0; 8], ReadKind::Unprivileged)
                .await
                .unwrap();
            cpu.write_memory(0x100, &[0; 8], true).await.unwrap();
            assert_eq!(
                support.translations,
                [
                    (
                        0x100,
                        if uao {
                            TranslateMode::Read
                        } else {
                            TranslateMode::ReadUnprivileged
                        }
                    ),
                    (
                        0x100,
                        if uao {
                            TranslateMode::Write
                        } else {
                            TranslateMode::WriteUnprivileged
                        }
                    ),
                ]
            );
        }
    }

    #[async_test]
    async fn syndrome_page_crossing_decodes_the_instruction_instead() {
        let gm = GuestMemory::allocate(0x4000);
        let value = 0x1234_5678_9abc_def0u64.to_le_bytes();
        gm.write_at(0xffd, &value[..3]).unwrap();
        gm.write_at(0x3000, &value[3..]).unwrap();
        let mut support = Support {
            initial: Some(InitialTranslation {
                gva: 0x4ffd,
                gpa: 0xffd,
                translate_mode: TranslateMode::Read,
            }),
            ..Default::default()
        };
        support.x[1] = 0x4ffd;
        let syndrome: EsrEl2 = IssDataAbort::new()
            .with_isv(true)
            .with_sas(3)
            .with_sf(true)
            .with_srt(0)
            .into();
        let intercept = InterceptState {
            instruction_bytes: 0xf940_0020u32.to_le_bytes(),
            instruction_byte_count: 4,
            gpa: Some(0xffd),
            syndrome: syndrome.with_il(true),
            ..Default::default()
        };
        emulate(&mut support, &intercept, &gm, &Io).await.unwrap();
        assert_eq!(support.x[0], u64::from_le_bytes(value));
        assert_eq!(support.pc, 4);
        assert!(support.events.is_empty());
        assert_eq!(support.translations, [(0x5000, TranslateMode::Read)]);
    }

    #[async_test]
    async fn big_endian_vectors_cross_pages_and_syndrome_reads() {
        let gm = GuestMemory::allocate(0x4000);
        let value = 0x1122_3344_5566_7788_99aa_bbcc_ddee_ff00u128;
        for el in [0, 1] {
            let mut support = Support {
                sctlr: aarch64defs::SctlrEl1::new().with_e0e(true).with_ee(true),
                ..Default::default()
            };
            support.cpsr.set_el(el);
            let mut cpu = EmulatorCpu::new(&gm, &Io, &mut support);
            cpu.write_memory(0x4ff8, &value.to_le_bytes(), false)
                .await
                .unwrap();
            let mut first = [0; 8];
            let mut second = [0; 8];
            gm.read_at(0xff8, &mut first).unwrap();
            gm.read_at(0x3000, &mut second).unwrap();
            assert_eq!(first, value.to_be_bytes()[..8]);
            assert_eq!(second, value.to_be_bytes()[8..]);
            let mut bytes = [0; 16];
            cpu.read_memory(0x4ff8, &mut bytes, ReadKind::Data)
                .await
                .unwrap();
            assert_eq!(u128::from_le_bytes(bytes), value);
        }
        gm.write_at(0x100, &[0x12, 0x34, 0x56, 0x78]).unwrap();
        let mut support = Support {
            sctlr: aarch64defs::SctlrEl1::new().with_ee(true),
            initial: Some(InitialTranslation {
                gva: 0x4100,
                gpa: 0x100,
                translate_mode: TranslateMode::Read,
            }),
            ..Default::default()
        };
        support.cpsr.set_el(1);
        let syndrome: EsrEl2 = IssDataAbort::new()
            .with_isv(true)
            .with_sas(2)
            .with_sf(false)
            .with_srt(0)
            .into();
        let intercept = InterceptState {
            gpa: Some(0x100),
            syndrome: syndrome.with_il(true),
            ..Default::default()
        };
        emulate(&mut support, &intercept, &gm, &Io).await.unwrap();
        assert_eq!(support.x[0], 0x1234_5678);
        assert_eq!(support.pc, 4);
    }

    #[async_test]
    async fn stack_alignment_fault_precedes_address_writeback() {
        let gm = GuestMemory::allocate(0x1000);
        for el in [0, 1] {
            let mut support = Support {
                sp: 0x108,
                sctlr: aarch64defs::SctlrEl1::new().with_sa(true).with_sa0(true),
                ..Default::default()
            };
            support.cpsr.set_el(el);
            let intercept = InterceptState {
                instruction_bytes: 0xf840_87e0u32.to_le_bytes(),
                instruction_byte_count: 4,
                ..Default::default()
            };
            emulate(&mut support, &intercept, &gm, &Io).await.unwrap();
            let event = exception(&support);
            assert_eq!(
                EsrEl2::from(event.syndrome).ec(),
                aarch64defs::ExceptionClass::SP_ALIGNMENT_FAULT.0
            );
            assert_eq!(support.sp, 0x108);
            assert_eq!(support.pc, 0);
            assert!(support.translations.is_empty());
        }
    }

    #[async_test]
    async fn alignment_and_undefined_instructions_inject_precise_events() {
        let gm = GuestMemory::allocate(0x1000);
        let mut support = Support {
            sctlr: aarch64defs::SctlrEl1::new().with_a(true),
            ..Default::default()
        };
        support.x[1] = 0x101;
        support.x[0] = 0xfeed;
        let intercept = InterceptState {
            instruction_bytes: 0xf940_0020u32.to_le_bytes(),
            instruction_byte_count: 4,
            ..Default::default()
        };
        emulate(&mut support, &intercept, &gm, &Io).await.unwrap();
        let event = exception(&support);
        let syndrome: EsrEl2 = event.syndrome.into();
        assert_eq!(syndrome.lower_iss(), FaultStatusCode::ALIGNMENT_FAULT.0);
        assert_eq!(event.fault_address, 0x101);
        assert_eq!(support.x[0], 0xfeed);
        assert_eq!(support.pc, 0);
        support.events.clear();
        let intercept = InterceptState {
            instruction_bytes: [0; 4],
            instruction_byte_count: 4,
            ..Default::default()
        };
        emulate(&mut support, &intercept, &gm, &Io).await.unwrap();
        assert_eq!(
            EsrEl2::from(exception(&support).syndrome).ec(),
            aarch64defs::ExceptionClass::UNKNOWN.0
        );
    }
}
