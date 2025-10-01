// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Wrapper around x86emu for emulating single instructions to handle VM exits.

use crate::translate::TranslateFlags;
use crate::translate::TranslatePrivilegeCheck;
use crate::translate::translate_gva_to_gpa;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvInterceptAccessType;
use hvdef::HvMapGpaFlags;
use thiserror::Error;
use virt::EmulatorMonitorSupport;
use virt::VpHaltReason;
use virt::io::CpuIo;
use vm_topology::processor::VpIndex;
use x86defs::Exception;
use x86defs::RFlags;
use x86defs::SegmentRegister;
use x86emu::AlignmentMode;
use x86emu::Gp;
use x86emu::RegisterIndex;
use x86emu::Segment;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

/// Support routines for the emulator.
pub trait EmulatorSupport {
    /// The current VP index.
    fn vp_index(&self) -> VpIndex;

    /// The processor vendor.
    fn vendor(&self) -> x86defs::cpuid::Vendor;

    /// Read a GP
    fn gp(&mut self, index: Gp) -> u64;

    /// Set a GP
    fn set_gp(&mut self, reg: Gp, v: u64);

    /// Read the instruction pointer
    fn rip(&mut self) -> u64;

    /// Set the instruction pointer
    fn set_rip(&mut self, v: u64);

    /// Read a segment register
    fn segment(&mut self, index: Segment) -> SegmentRegister;

    /// Read the efer
    fn efer(&mut self) -> u64;

    /// Read cr0
    fn cr0(&mut self) -> u64;

    /// Read rflags
    fn rflags(&mut self) -> RFlags;

    /// Set rflags
    fn set_rflags(&mut self, v: RFlags);

    /// Gets the value of an XMM* register.
    fn xmm(&mut self, reg: usize) -> u128;

    /// Sets the value of an XMM* register.
    fn set_xmm(&mut self, reg: usize, value: u128);

    /// Flush registers in the emulation cache to the backing
    fn flush(&mut self);

    /// The instruction bytes, if available.
    fn instruction_bytes(&self) -> &[u8];

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
    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent);

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

    /// Returns the page-aligned base address of the enabled local APIC in xapic
    /// mode.
    fn lapic_base_address(&self) -> Option<u64>;

    /// Read from the current processor's local APIC memory mapped interface.
    ///
    /// This will only be called on an address in the page returned by
    /// `lapic_base_address`.
    fn lapic_read(&mut self, address: u64, data: &mut [u8]);

    /// Write to the current processor's local APIC memory mapped interface.
    ///
    /// This will only be called on an address in the page returned by
    /// `lapic_base_address`.
    fn lapic_write(&mut self, address: u64, data: &[u8]);
}

pub trait TranslateGvaSupport {
    /// Gets the object used to access the guest memory.
    fn guest_memory(&self) -> &GuestMemory;

    /// Acquires the TLB lock for this processor.
    fn acquire_tlb_lock(&mut self);

    /// Returns the registers used to walk the page table.
    fn registers(&mut self) -> crate::translate::TranslationRegisters;
}

/// Emulates a page table walk.
///
/// This is suitable for implementing [`EmulatorSupport::translate_gva`].
pub fn emulate_translate_gva<T: TranslateGvaSupport>(
    support: &mut T,
    gva: u64,
    mode: TranslateMode,
) -> Result<EmuTranslateResult, EmuTranslateError> {
    // Always acquire the TLB lock for this path.
    support.acquire_tlb_lock();

    let flags = TranslateFlags {
        validate_execute: matches!(mode, TranslateMode::Execute),
        validate_read: matches!(mode, TranslateMode::Read | TranslateMode::Write),
        validate_write: matches!(mode, TranslateMode::Write),
        override_smap: false,
        enforce_smap: false,
        privilege_check: TranslatePrivilegeCheck::CurrentPrivilegeLevel,
        set_page_table_bits: true,
    };

    let registers = support.registers();

    match translate_gva_to_gpa(support.guest_memory(), gva, &registers, flags) {
        Ok(crate::translate::TranslateResult { gpa, cache_info: _ }) => Ok(EmuTranslateResult {
            gpa,
            overlay_page: None,
        }),
        Err(err) => Err(EmuTranslateError {
            code: err.into(),
            event_info: None,
        }),
    }
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
    pub event_info: Option<hvdef::HvX64PendingEvent>,
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
    #[error("linear IP was not within CS segment limit")]
    LinearIpPastCsLimit,
    #[error("failed to read instruction stream")]
    InstructionRead(#[source] Error),
    #[error("emulator error (instruction {bytes:02x?})")]
    Emulator {
        bytes: Vec<u8>,
        #[source]
        error: x86emu::Error<Error>,
    },
}

pub struct EmulatorMemoryAccess<'a> {
    pub gm: &'a GuestMemory,
    pub kx_gm: &'a GuestMemory,
    pub ux_gm: &'a GuestMemory,
}

enum EmulatorMemoryAccessType {
    ReadWrite,
    InstructionRead { is_user_mode: bool },
}

impl EmulatorMemoryAccess<'_> {
    fn gm(&self, access_type: EmulatorMemoryAccessType) -> &GuestMemory {
        match access_type {
            EmulatorMemoryAccessType::ReadWrite => self.gm,
            EmulatorMemoryAccessType::InstructionRead { is_user_mode } => {
                if is_user_mode {
                    self.ux_gm
                } else {
                    self.kx_gm
                }
            }
        }
    }
}

/// Emulates an instruction.
pub async fn emulate<T: EmulatorSupport>(
    support: &mut T,
    emu_mem: &EmulatorMemoryAccess<'_>,
    dev: &impl CpuIo,
) -> Result<(), VpHaltReason> {
    emulate_core(support, emu_mem, dev)
        .await
        .map_err(|e| dev.fatal_error(e.into()))
}

async fn emulate_core<T: EmulatorSupport>(
    support: &mut T,
    emu_mem: &EmulatorMemoryAccess<'_>,
    dev: &impl CpuIo,
) -> Result<(), EmulationError> {
    let vendor = support.vendor();

    let mut bytes = [0; 16];
    let mut valid_bytes;
    {
        let instruction_bytes = support.instruction_bytes();
        valid_bytes = instruction_bytes.len();
        bytes[..valid_bytes].copy_from_slice(instruction_bytes);
    }
    let instruction_bytes = &bytes[..valid_bytes];

    tracing::trace!(
        ?instruction_bytes,
        physical_address = support.physical_address(),
        "emulating"
    );

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

    let initial_alignment_check = support.rflags().alignment_check();

    let mut cpu = EmulatorCpu::new(
        emu_mem.gm(EmulatorMemoryAccessType::ReadWrite),
        dev,
        support,
    );
    let result = loop {
        let instruction_bytes = &bytes[..valid_bytes];
        let mut emu = x86emu::Emulator::new(&mut cpu, vendor, instruction_bytes);
        let res = emu.run().await;

        if let Err(e) = &res {
            if let x86emu::Error::NotEnoughBytes = **e {
                assert!(valid_bytes < bytes.len());

                // TODO: inject #GP due to segmentation fault.
                let linear_ip = emu
                    .linear_ip(valid_bytes as u64)
                    .ok_or(EmulationError::LinearIpPastCsLimit)?;

                let is_user_mode = emu.is_user_mode();

                let translate_result =
                    cpu.translate_gva(linear_ip, TranslateMode::Execute, is_user_mode);

                let phys_ip = match translate_result {
                    Ok(ip) => ip,
                    Err(translate_error) => {
                        if inject_memory_access_fault(linear_ip, &translate_error, support) {
                            return Ok(());
                        } else {
                            return Err(EmulationError::InstructionRead(translate_error));
                        }
                    }
                };

                // TODO: fold this access check into the GuestMemory object for
                // each of the backings, if possible.
                if let Err(err) = cpu.check_vtl_access(phys_ip, TranslateMode::Execute) {
                    if inject_memory_access_fault(linear_ip, &err, support) {
                        return Ok(());
                    } else {
                        return Err(EmulationError::InstructionRead(err));
                    };
                }

                tracing::trace!(linear_ip, phys_ip, "fetching instruction bytes");

                let len = (bytes.len() - valid_bytes)
                    .min((HV_PAGE_SIZE - (phys_ip & (HV_PAGE_SIZE - 1))) as usize);

                let instruction_gm =
                    emu_mem.gm(EmulatorMemoryAccessType::InstructionRead { is_user_mode });

                if let Err(err) =
                    instruction_gm.read_at(phys_ip, &mut bytes[valid_bytes..valid_bytes + len])
                {
                    tracing::error!(error = &err as &dyn std::error::Error, "read failed");
                    support.inject_pending_event(gpf_event());
                    return Ok(());
                }

                valid_bytes += len;
                continue;
            }
        }

        break res;
    };

    cpu.support.flush();

    // If the alignment check flag is not in sync with the hypervisor because the instruction emulator
    // modifies internally, then the appropriate SMAP enforcement flags need to be passed to the hypervisor
    // during the translation of gvas to gpa.
    //
    // Note: also applies if the instruction emulator emulates instructions resulting in implicit
    // memory accesses, which is currently not done. See Intel Spec 4.6 Access Rights:
    // "Some operations implicitly access system data structures with linear addresses;
    // the resulting accesses to those data structures are supervisor-mode accesses regardless of CPL.
    // Examples of such accesses include the following: accesses to the global descriptor table (GDT)
    // or local descriptor table (LDT) to load a segment descriptor; accesses to the interrupt
    // descriptor table (IDT) when delivering an interrupt or exception; and accesses to the task-state
    // segment (TSS) as part of a task switch or change of CPL."
    assert_eq!(
        initial_alignment_check,
        cpu.support.rflags().alignment_check()
    );

    let instruction_bytes = &bytes[..valid_bytes];
    if let Err(e) = result {
        match *e {
            err @ (x86emu::Error::DecodeFailure | x86emu::Error::UnsupportedInstruction { .. }) => {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    ?instruction_bytes,
                    physical_address = cpu.support.physical_address(),
                    "unsupported instruction"
                );

                cpu.support.inject_pending_event(make_exception_event(
                    Exception::INVALID_OPCODE,
                    None,
                    None,
                ));
            }
            err @ x86emu::Error::NonMemoryOrPortInstruction { .. } => {
                tracelimit::error_ratelimited!(
                    error = &err as &dyn std::error::Error,
                    ?instruction_bytes,
                    physical_address = cpu.support.physical_address(),
                    "given an instruction that we shouldn't have been asked to emulate - likely a bug in the caller"
                );

                return Err(EmulationError::Emulator {
                    bytes: instruction_bytes.to_vec(),
                    error: err,
                });
            }
            x86emu::Error::InstructionException(exception, error_code, cause) => {
                tracing::trace!(
                    ?exception,
                    ?error_code,
                    ?cause,
                    "emulated instruction caused exception"
                );
                cpu.support
                    .inject_pending_event(make_exception_event(exception, error_code, None));
            }
            x86emu::Error::MemoryAccess(addr, kind, err) => {
                if !inject_memory_access_fault(addr, &err, support) {
                    return Err(EmulationError::Emulator {
                        bytes: instruction_bytes.to_vec(),
                        error: x86emu::Error::MemoryAccess(addr, kind, err),
                    });
                }
            }
            err @ x86emu::Error::IoPort { .. } => {
                return Err(EmulationError::Emulator {
                    bytes: instruction_bytes.to_vec(),
                    error: err,
                });
            }
            x86emu::Error::NotEnoughBytes => unreachable!(),
        }
    }

    Ok(())
}

/// Performs a memory operation as if it had been performed by an emulated instruction.
///
/// "As if it had been performed by an emulated instruction" means that the given
/// GVA will be translated to a GPA, subject to applicable segmentation, permission,
/// and alignment checks, may be determined to be MMIO instead of RAM, etc.
pub async fn emulate_insn_memory_op<T: EmulatorSupport>(
    support: &mut T,
    gm: &GuestMemory,
    dev: &impl CpuIo,
    gva: u64,
    segment: Segment,
    alignment: AlignmentMode,
    op: EmulatedMemoryOperation<'_>,
) -> Result<(), VpHaltReason> {
    assert!(!support.interruption_pending());

    let vendor = support.vendor();
    let mut cpu = EmulatorCpu::new(gm, dev, support);
    let mut emu = x86emu::Emulator::new(&mut cpu, vendor, &[]);

    match op {
        EmulatedMemoryOperation::Read(data) => emu.read_memory(segment, gva, alignment, data).await,
        EmulatedMemoryOperation::Write(data) => {
            emu.write_memory(segment, gva, alignment, data).await
        }
    }
    .map_err(|e| dev.fatal_error(e.into()))

    // No need to flush the cache, we have not modified any registers.
}

pub enum EmulatedMemoryOperation<'a> {
    Read(&'a mut [u8]),
    Write(&'a [u8]),
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
    // TODO: this should be able to hold at least two entries for effective use for
    // rep move instructions.
    cached_translation: Option<GvaGpaCacheEntry>,
}

#[derive(Debug, Error)]
enum Error {
    #[error("translation error")]
    Translate(
        #[source] TranslateGvaError,
        Option<hvdef::HvX64PendingEvent>,
    ),
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

    pub fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
        is_user_mode: bool,
    ) -> Result<u64, Error> {
        type TranslateCode = hvdef::hypercall::TranslateGvaResultCode;

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
                    // We will support reads to overlay pages in order to support Win2k3
                    // crash dumps (which do direct port io to the ide for all of memory,
                    // including overlay pages).  Writes, though, are right out.  There is
                    // no known scenario where guests should be writing to overlay pages,
                    // and doing so would be difficult and expensive.  Overlay pages are
                    // special purpose pages set up by the hypervisor (to implement hypercalls,
                    // for instance), and there should be no reason that guests should be doing
                    // io to them.
                    //
                    // At this time, there is no infrastructure to allow us to actually
                    // read the overlay page.  We will instead return 0xff's for such reads.
                    // This is an emulation hole.  It is felt to be acceptable at this time.
                    // But for this reason, we give up if this wasn't the intercepting instruction
                    // and let the processor run the instruction directly.
                    return Err(Error::Translate(
                        TranslateGvaError::OverlayPageWrite,
                        Some(gpf_event()),
                    ));
                }

                let new_cache_entry = GvaGpaCacheEntry::new(gva, gpa, mode);

                self.cached_translation = Some(new_cache_entry);
                Ok(gpa)
            }
            Err(EmuTranslateError { code, event_info }) => {
                match code {
                    TranslateCode::INTERCEPT => {
                        tracing::trace!("translate gva to gpa returned an intercept event");
                        Err(Error::Translate(TranslateGvaError::Intercept, event_info))
                    }
                    TranslateCode::GPA_NO_READ_ACCESS
                    | TranslateCode::GPA_NO_WRITE_ACCESS
                    | TranslateCode::GPA_UNMAPPED
                    | TranslateCode::GPA_ILLEGAL_OVERLAY_ACCESS
                    | TranslateCode::GPA_UNACCEPTED => {
                        // The page table walk failed because one of the page
                        // table entries was inaccessible in the second-level
                        // page tables.
                        //
                        // Inject a #GP.
                        tracing::trace!(
                            "translate gva to gpa returned no access to page {:?}",
                            code
                        );
                        Err(Error::Translate(
                            TranslateGvaError::AccessDenied(code),
                            Some(gpf_event()),
                        ))
                    }
                    TranslateCode::PAGE_NOT_PRESENT
                    | TranslateCode::PRIVILEGE_VIOLATION
                    | TranslateCode::INVALID_PAGE_TABLE_FLAGS => {
                        // The page table walk failed for ordinary reasons not
                        // having to do with second-level address translation.
                        // We need to inject a page fault.
                        //
                        // It should be rare to get to this point even for a
                        // misbehaving guest, since the processor usually should
                        // have detected and injected this fault without
                        // requiring an exit.
                        //
                        // Trace since this is more likely to indicate a bug in
                        // our page table walking code, but rate limit the trace
                        // since there are still cases where this could be
                        // triggered by guest behavior.
                        tracelimit::warn_ratelimited!(gva, ?code, "page table walk failed");

                        let mut error = x86defs::PageFaultErrorCode::new();
                        match code {
                            TranslateCode::PAGE_NOT_PRESENT => (),
                            TranslateCode::PRIVILEGE_VIOLATION => error.set_present(true),
                            TranslateCode::INVALID_PAGE_TABLE_FLAGS => {
                                error.set_present(true);
                                error.set_reserved(true);
                            }
                            _ => unreachable!(),
                        };

                        match mode {
                            TranslateMode::Execute => error.set_fetch(true),
                            TranslateMode::Write => error.set_write(true),
                            _ => (),
                        };

                        if is_user_mode {
                            error.set_user(true);
                        }

                        // Page fault
                        let event = make_exception_event(
                            Exception::PAGE_FAULT,
                            Some(error.into()),
                            Some(gva),
                        );

                        Err(Error::Translate(
                            TranslateGvaError::PageFault(code),
                            Some(event),
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
                }
            }
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

impl<T: EmulatorSupport, U: CpuIo> x86emu::Cpu for EmulatorCpu<'_, T, U> {
    type Error = Error;

    async fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        let gpa = self.translate_gva(gva, TranslateMode::Read, is_user_mode)?;

        if Some(gpa & !0xfff) == self.support.lapic_base_address() {
            self.support.lapic_read(gpa, bytes);
            return Ok(());
        }

        self.check_vtl_access(gpa, TranslateMode::Read)?;

        if self.check_monitor_read(gpa, bytes) {
            Ok(())
        } else if self.support.is_gpa_mapped(gpa, false) {
            self.gm.read_at(gpa, bytes).map_err(Error::Memory)
        } else {
            self.dev
                .read_mmio(self.support.vp_index(), gpa, bytes)
                .await;
            Ok(())
        }
    }

    async fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
        is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        let gpa = self.translate_gva(gva, TranslateMode::Write, is_user_mode)?;

        if Some(gpa & !0xfff) == self.support.lapic_base_address() {
            self.support.lapic_write(gpa, bytes);
            return Ok(());
        }

        self.check_vtl_access(gpa, TranslateMode::Write)?;

        if self.support.is_gpa_mapped(gpa, true) {
            self.gm.write_at(gpa, bytes).map_err(Error::Memory)?;
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
        is_user_mode: bool,
    ) -> Result<bool, Self::Error> {
        let gpa = self.translate_gva(gva, TranslateMode::Write, is_user_mode)?;
        self.check_vtl_access(gpa, TranslateMode::Write)?;

        let success = if self.check_monitor_write(gpa, new) {
            true
        } else if self.support.is_gpa_mapped(gpa, true) {
            match (current.len(), new.len()) {
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
                _ => panic!("unsupported cmpxchg size"),
            }
            .map_err(Error::Memory)?
        } else {
            // Ignore the comparison aspect for device MMIO.
            self.dev.write_mmio(self.support.vp_index(), gpa, new).await;
            true
        };
        Ok(success)
    }

    async fn read_io(&mut self, io_port: u16, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.dev
            .read_io(self.support.vp_index(), io_port, bytes)
            .await;
        Ok(())
    }

    async fn write_io(&mut self, io_port: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        self.dev
            .write_io(self.support.vp_index(), io_port, bytes)
            .await;
        Ok(())
    }

    fn gp(&mut self, reg: RegisterIndex) -> u64 {
        let extended_register = self.support.gp(reg.extended_index);
        reg.apply_sizing(extended_register)
    }

    fn gp_sign_extend(&mut self, reg: RegisterIndex) -> i64 {
        let extended_register = self.support.gp(reg.extended_index);
        reg.apply_sizing_signed(extended_register)
    }

    fn set_gp(&mut self, reg: RegisterIndex, v: u64) {
        let register_value = self.gp(reg);
        let updated_register_value = reg.apply_update(register_value, v);
        self.support
            .set_gp(reg.extended_index, updated_register_value);
    }

    fn rip(&mut self) -> u64 {
        self.support.rip()
    }

    fn set_rip(&mut self, v: u64) {
        self.support.set_rip(v);
    }

    fn segment(&mut self, index: Segment) -> SegmentRegister {
        self.support.segment(index)
    }

    fn efer(&mut self) -> u64 {
        self.support.efer()
    }

    fn cr0(&mut self) -> u64 {
        self.support.cr0()
    }

    fn rflags(&mut self) -> RFlags {
        self.support.rflags()
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.support.set_rflags(v);
    }

    /// Gets the value of an XMM* register.
    fn xmm(&mut self, reg: usize) -> u128 {
        self.support.xmm(reg)
    }

    /// Sets the value of an XMM* register.
    fn set_xmm(&mut self, reg: usize, value: u128) {
        self.support.set_xmm(reg, value)
    }
}

/// Emulates an IO port instruction.
///
/// Just handles calling into the IO bus and updating `rax`. The caller must
/// update RIP, and it must update the VP's `rax` register (when `!is_write`).
///
/// The caller is also responsible for performing any security checks to ensure
/// the guest is allowed to execute I/O instructions. However, typically this is handled
/// by the hardware and hypervisor automatically.
pub async fn emulate_io(
    vp_index: VpIndex,
    is_write: bool,
    port: u16,
    rax: &mut u64,
    len: u8,
    dev: &impl CpuIo,
) {
    let len = len as usize;
    if is_write {
        dev.write_io(vp_index, port, &rax.to_ne_bytes()[..len])
            .await;
    } else {
        // Preserve the high bits of eax but not of rax.
        let mut value = (*rax as u32).to_ne_bytes();
        dev.read_io(vp_index, port, &mut value[..len]).await;
        *rax = u32::from_ne_bytes(value) as u64;
    }
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
) -> bool {
    match result {
        Error::Translate(e, event) => {
            tracing::trace!(
                error = e as &dyn std::error::Error,
                "translation failed, injecting event"
            );

            if let Some(event_info) = event {
                support.inject_pending_event(*event_info);

                // The emulation did what it was supposed to do, which is throw a fault, so the emulation is done.
                return true;
            }
            false
        }
        Error::NoVtlAccess {
            gpa,
            intercepting_vtl,
            denied_flags,
        } => {
            tracing::trace!(
                error = result as &dyn std::error::Error,
                ?gva,
                ?gpa,
                "Vtl permissions checking failed"
            );

            let event = vtl_access_event(gva, *gpa, *intercepting_vtl, *denied_flags);
            support.inject_pending_event(event);
            true
        }
        Error::Memory(_) => false,
    }
}

/// Creates a pending event for the exception type
fn make_exception_event(
    exception: Exception,
    error_code: Option<u32>,
    exception_parameter: Option<u64>,
) -> hvdef::HvX64PendingEvent {
    let exception_event = hvdef::HvX64PendingExceptionEvent::new()
        .with_event_pending(true)
        .with_event_type(hvdef::HV_X64_PENDING_EVENT_EXCEPTION)
        .with_deliver_error_code(error_code.is_some())
        .with_error_code(error_code.unwrap_or(0))
        .with_vector(exception.0.into())
        .with_exception_parameter(exception_parameter.unwrap_or(0));

    hvdef::HvX64PendingEvent::from(exception_event)
}

/// Generates a general protection fault pending event
fn gpf_event() -> hvdef::HvX64PendingEvent {
    make_exception_event(Exception::GENERAL_PROTECTION_FAULT, Some(0), None)
}

/// Generates the appropriate event for a VTL access error based
/// on the intercepting VTL
fn vtl_access_event(
    gva: u64,
    gpa: u64,
    intercepting_vtl: hvdef::Vtl,
    denied_access: HvMapGpaFlags,
) -> hvdef::HvX64PendingEvent {
    if intercepting_vtl != hvdef::Vtl::Vtl2 {
        let event_header = hvdef::HvX64PendingEventMemoryInterceptPendingEventHeader::new()
            .with_event_pending(true)
            .with_event_type(hvdef::HV_X64_PENDING_EVENT_MEMORY_INTERCEPT);
        let access_flags = hvdef::HvX64PendingEventMemoryInterceptAccessFlags::new()
            .with_guest_linear_address_valid(true)
            .with_caused_by_gpa_access(true);

        let access_type = if denied_access.kernel_executable() || denied_access.user_executable() {
            HvInterceptAccessType::EXECUTE
        } else if denied_access.writable() {
            HvInterceptAccessType::WRITE
        } else {
            HvInterceptAccessType::READ
        };

        let memory_event = hvdef::HvX64PendingEventMemoryIntercept {
            event_header,
            target_vtl: intercepting_vtl.into(),
            access_type,
            access_flags,
            _reserved2: 0,
            guest_linear_address: (gva >> hvdef::HV_PAGE_SHIFT) << hvdef::HV_PAGE_SHIFT,
            guest_physical_address: (gpa >> hvdef::HV_PAGE_SHIFT) << hvdef::HV_PAGE_SHIFT,
            _reserved3: 0,
        };

        hvdef::HvX64PendingEvent::read_from_bytes(memory_event.as_bytes())
            .expect("memory event and pending event should be the same size")
    } else {
        gpf_event()
    }
}

/// Tries to emulate monitor page writes without taking the slower, full
/// emulation path.
///
/// The caller must have already validated that the fault was due to a write to
/// a monitor page GPA.
///
/// Returns the bit number being set within the monitor page.
pub fn emulate_mnf_write_fast_path<T: EmulatorSupport>(
    support: &mut T,
    gm: &GuestMemory,
    dev: &impl CpuIo,
    interruption_pending: bool,
    tlb_lock_held: bool,
) -> Option<u32> {
    let mut cpu = EmulatorCpu::new(gm, dev, support);
    let instruction_bytes = cpu.support.instruction_bytes();
    if interruption_pending || !tlb_lock_held || instruction_bytes.is_empty() {
        return None;
    }
    let mut bytes = [0; 16];
    let valid_bytes;
    {
        let instruction_bytes = cpu.support.instruction_bytes();
        valid_bytes = instruction_bytes.len();
        bytes[..valid_bytes].copy_from_slice(instruction_bytes);
    }
    let instruction_bytes = &bytes[..valid_bytes];
    let bit = x86emu::fast_path::emulate_fast_path_set_bit(instruction_bytes, &mut cpu);
    support.flush();
    bit
}
