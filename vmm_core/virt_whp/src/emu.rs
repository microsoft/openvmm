// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(guest_arch = "x86_64")]

use crate::WhpProcessor;
use crate::memory::x86::GpaBackingType;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use virt::VpIndex;
use virt::io::CpuIo;
use virt_support_x86emu::emulate::EmuTranslateError;
use virt_support_x86emu::emulate::EmuTranslateResult;
use virt_support_x86emu::emulate::TranslateGvaSupport;
use virt_support_x86emu::emulate::TranslateMode;
use virt_support_x86emu::emulate::emulate_translate_gva;
use virt_support_x86emu::translate::TranslationRegisters;
use x86defs::RFlags;
use x86defs::SegmentRegister;

pub(crate) enum WhpVpRefEmulation<'a> {
    MemoryAccessContext(&'a whp::abi::WHV_MEMORY_ACCESS_CONTEXT),
    IoPortAccessContext(&'a whp::abi::WHV_X64_IO_PORT_ACCESS_CONTEXT),
}

pub(crate) struct WhpEmulationState<'a, 'b, T: CpuIo> {
    access: &'a WhpVpRefEmulation<'a>,
    vp: &'a mut WhpProcessor<'b>,
    interruption_pending: bool,
    dev: &'a T,
    cache: WhpEmuCache,
}

pub(crate) struct WhpEmuCache {
    /// GP registers, in the canonical order (as defined by `RAX`, etc.).
    gps: [u64; 16],
    /// Segment registers, in the canonical order (as defined by `ES`, etc.).
    segs: [SegmentRegister; 6],
    rip: u64,
    rflags: RFlags,

    cr0: u64,
    efer: u64,
}

impl<'a, 'b, T: CpuIo> WhpEmulationState<'a, 'b, T> {
    pub fn new(
        access: &'a WhpVpRefEmulation<'a>,
        vp: &'a mut WhpProcessor<'b>,
        exit: &whp::Exit<'_>,
        dev: &'a T,
    ) -> Self {
        let interruption_pending = exit.vp_context.ExecutionState.InterruptionPending();
        let cache = vp.emulator_state();
        Self {
            access,
            vp,
            interruption_pending,
            dev,
            cache,
        }
    }
}

impl<T: CpuIo> virt_support_x86emu::emulate::EmulatorSupport for WhpEmulationState<'_, '_, T> {
    fn vp_index(&self) -> VpIndex {
        self.vp.vp.index
    }

    fn vendor(&self) -> x86defs::cpuid::Vendor {
        self.vp.vp.partition.caps.vendor
    }

    fn gp(&mut self, reg: x86emu::Gp) -> u64 {
        self.cache.gps[reg as usize]
    }

    fn set_gp(&mut self, reg: x86emu::Gp, v: u64) {
        self.cache.gps[reg as usize] = v;
    }

    fn rip(&mut self) -> u64 {
        self.cache.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.cache.rip = v;
    }

    fn segment(&mut self, reg: x86emu::Segment) -> SegmentRegister {
        self.cache.segs[reg as usize]
    }

    fn efer(&mut self) -> u64 {
        self.cache.efer
    }

    fn cr0(&mut self) -> u64 {
        self.cache.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.cache.rflags
    }

    fn set_rflags(&mut self, v: RFlags) {
        self.cache.rflags = v;
    }

    fn xmm(&mut self, reg: usize) -> u128 {
        assert!(reg < 16);
        let reg = whp::abi::WHV_REGISTER_NAME(whp::abi::WHvX64RegisterXmm0.0 + reg as u32);
        let mut value = [Default::default()];
        let _ = self.vp.current_whp().get_registers(&[reg], &mut value);
        value[0].0.into()
    }

    fn set_xmm(&mut self, reg: usize, value: u128) {
        assert!(reg < 16);
        let reg = whp::abi::WHV_REGISTER_NAME(whp::abi::WHvX64RegisterXmm0.0 + reg as u32);
        let value = [whp::abi::WHV_REGISTER_VALUE(value.into())];
        self.vp.current_whp().set_registers(&[reg], &value).unwrap();
    }

    fn flush(&mut self) {
        self.vp.set_emulator_state(&self.cache);
    }

    /// Check if the given gpa is accessible by the current VTL.
    fn check_vtl_access(
        &mut self,
        gpa: u64,
        mode: TranslateMode,
    ) -> Result<(), virt_support_x86emu::emulate::EmuCheckVtlAccessError> {
        match &self.vp.vp.partition.vtl2_emulation {
            Some(vtl2_emulation) => {
                let gpa_page = gpa / HV_PAGE_SIZE;
                let active_vtl = self.vp.state.active_vtl;

                // If this is a lower VTL, then it cannot access VTL2 protected pages.
                if active_vtl != Vtl::Vtl2
                    && vtl2_emulation.protected_pages.read().contains(&gpa_page)
                {
                    let vtl_flags = match mode {
                        TranslateMode::Read => hvdef::HvMapGpaFlags::new().with_readable(true),
                        TranslateMode::Write => hvdef::HvMapGpaFlags::new().with_writable(true),
                        TranslateMode::Execute => hvdef::HvMapGpaFlags::new()
                            .with_kernel_executable(true)
                            .with_user_executable(true),
                    };

                    tracing::error!(
                        gpa_page,
                        ?active_vtl,
                        ?mode,
                        "lower VTL attempted to access restricted page"
                    );
                    Err(
                        virt_support_x86emu::emulate::EmuCheckVtlAccessError::AccessDenied {
                            vtl: Vtl::Vtl2,
                            denied_flags: vtl_flags,
                        },
                    )
                } else {
                    Ok(())
                }
            }
            // No vtl2 means always valid.
            None => Ok(()),
        }
    }

    fn instruction_bytes(&self) -> &[u8] {
        let (bytes, count) = match self.access {
            WhpVpRefEmulation::MemoryAccessContext(access) => {
                (&access.InstructionBytes, access.InstructionByteCount)
            }
            WhpVpRefEmulation::IoPortAccessContext(access) => {
                (&access.InstructionBytes, access.InstructionByteCount)
            }
        };
        &bytes[..count as usize]
    }

    fn physical_address(&self) -> Option<u64> {
        match self.access {
            WhpVpRefEmulation::MemoryAccessContext(access) => Some(access.Gpa),
            WhpVpRefEmulation::IoPortAccessContext(_) => None,
        }
    }

    fn initial_gva_translation(
        &mut self,
    ) -> Option<virt_support_x86emu::emulate::InitialTranslation> {
        if let WhpVpRefEmulation::MemoryAccessContext(access) = self.access {
            if !(access.AccessInfo.GvaValid()) {
                return None;
            }

            if let Ok(translate_mode) = TranslateMode::try_from(hvdef::HvInterceptAccessType(
                access.AccessInfo.AccessType().0 as u8,
            )) {
                return Some(virt_support_x86emu::emulate::InitialTranslation {
                    gva: access.Gva,
                    gpa: access.Gpa,
                    translate_mode,
                });
            }
        }

        None
    }

    fn interruption_pending(&self) -> bool {
        self.interruption_pending
    }

    fn translate_gva(
        &mut self,
        gva: u64,
        mode: TranslateMode,
    ) -> Result<EmuTranslateResult, EmuTranslateError> {
        emulate_translate_gva(self, gva, mode)
    }

    fn inject_pending_event(&mut self, event_info: hvdef::HvX64PendingEvent) {
        whp::set_registers!(
            self.vp.current_whp(),
            [(whp::Register128::PendingEvent, event_info.reg_0.into()),]
        )
        .expect("set registers should not fail");

        // PendingEvent1 may not exist if we're running on an older version of
        // Windows, so only attempt to set it when necessary. This should be
        // rare, as it should only be set when nested virtualization is enabled.
        // TODO: If it does need to be set, and fails, just panic for now.
        // We'll figure something better out later.
        if event_info.reg_1 != 0u128.into() {
            whp::set_registers!(
                self.vp.current_whp(),
                [(
                    whp::Register128::PendingEvent1,
                    u128::from(event_info.reg_1)
                ),]
            )
            .expect("set registers should not fail");
        }
    }

    fn monitor_support(&self) -> Option<&dyn virt::EmulatorMonitorSupport> {
        Some(self)
    }

    fn is_gpa_mapped(&self, gpa: u64, write: bool) -> bool {
        // Skip the slightly more expensive step if the hypervisor already told
        // us that this GPA is not mapped.
        if let WhpVpRefEmulation::MemoryAccessContext(ctx) = self.access {
            if ctx.Gpa == gpa && ctx.AccessInfo.GpaUnmapped() {
                return false;
            }
        }

        match self
            .vp
            .vp
            .partition
            .gpa_backing_type(self.vp.state.active_vtl, gpa)
        {
            GpaBackingType::MonitorPage => !write,
            GpaBackingType::Ram { writable } => {
                if write {
                    writable
                } else {
                    true
                }
            }
            GpaBackingType::Unmapped => false,
            GpaBackingType::VtlProtected(_) => panic!(
                "vtl protected page should not be emulated but intercept message into higher vtl"
            ),
            GpaBackingType::Unaccepted => {
                panic!("unaccepted page should be injected as intercept message {gpa:x}")
            }
        }
    }

    fn lapic_base_address(&self) -> Option<u64> {
        self.vp.state.vtls[self.vp.state.active_vtl]
            .lapic
            .as_ref()
            .and_then(|lapic| lapic.apic.base_address())
    }

    fn lapic_read(&mut self, address: u64, data: &mut [u8]) {
        self.vp.apic_read(address, data, self.dev)
    }

    fn lapic_write(&mut self, address: u64, data: &[u8]) {
        self.vp.apic_write(address, data, self.dev)
    }
}

impl<T: CpuIo> virt::EmulatorMonitorSupport for WhpEmulationState<'_, '_, T> {
    fn check_write(&self, gpa: u64, bytes: &[u8]) -> bool {
        self.vp
            .vp
            .partition
            .monitor_page
            .check_write(gpa, bytes, |connection_id| {
                self.vp.signal_mnf(self.dev, connection_id)
            })
    }

    fn check_read(&self, gpa: u64, bytes: &mut [u8]) -> bool {
        self.vp.vp.partition.monitor_page.check_read(gpa, bytes)
    }
}

impl<T: CpuIo> TranslateGvaSupport for WhpEmulationState<'_, '_, T> {
    fn guest_memory(&self) -> &guestmem::GuestMemory {
        &self.vp.vp.partition.gm
    }

    fn acquire_tlb_lock(&mut self) {
        // Nothing to do here for exo partitions: the TLB lock is always held
        // while exited.
    }

    fn registers(&mut self) -> TranslationRegisters {
        self.vp.translation_registers(self.vp.state.active_vtl)
    }
}

fn from_seg_reg(reg: &whp::abi::WHV_X64_SEGMENT_REGISTER) -> SegmentRegister {
    SegmentRegister {
        base: reg.Base,
        limit: reg.Limit,
        selector: reg.Selector,
        attributes: reg.Attributes.into(),
    }
}

impl WhpProcessor<'_> {
    pub(crate) fn emulator_state(&mut self) -> WhpEmuCache {
        let (
            rip,
            rflags,
            rax,
            rcx,
            rdx,
            rbx,
            rsp,
            rbp,
            rsi,
            rdi,
            r8,
            r9,
            r10,
            r11,
            r12,
            r13,
            r14,
            r15,
            es,
            cs,
            ds,
            fs,
            gs,
            ss,
            cr0,
            efer,
        ) = whp::get_registers!(
            self.current_whp(),
            [
                whp::Register64::Rip,
                whp::Register64::Rflags,
                whp::Register64::Rax,
                whp::Register64::Rcx,
                whp::Register64::Rdx,
                whp::Register64::Rbx,
                whp::Register64::Rsp,
                whp::Register64::Rbp,
                whp::Register64::Rsi,
                whp::Register64::Rdi,
                whp::Register64::R8,
                whp::Register64::R9,
                whp::Register64::R10,
                whp::Register64::R11,
                whp::Register64::R12,
                whp::Register64::R13,
                whp::Register64::R14,
                whp::Register64::R15,
                whp::RegisterSegment::Es,
                whp::RegisterSegment::Cs,
                whp::RegisterSegment::Ds,
                whp::RegisterSegment::Fs,
                whp::RegisterSegment::Gs,
                whp::RegisterSegment::Ss,
                whp::Register64::Cr0,
                whp::Register64::Efer,
            ]
        )
        .unwrap();

        WhpEmuCache {
            gps: [
                rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8, r9, r10, r11, r12, r13, r14, r15,
            ],
            segs: [
                from_seg_reg(&es),
                from_seg_reg(&cs),
                from_seg_reg(&ss),
                from_seg_reg(&ds),
                from_seg_reg(&fs),
                from_seg_reg(&gs),
            ],
            rip,
            rflags: rflags.into(),
            cr0,
            efer,
        }
    }

    pub(crate) fn set_emulator_state(&mut self, state: &WhpEmuCache) {
        whp::set_registers!(
            self.current_whp(),
            [
                (whp::Register64::Rip, state.rip),
                (whp::Register64::Rflags, state.rflags.into()),
                (whp::Register64::Rax, state.gps[0]),
                (whp::Register64::Rcx, state.gps[1]),
                (whp::Register64::Rdx, state.gps[2]),
                (whp::Register64::Rbx, state.gps[3]),
                (whp::Register64::Rsp, state.gps[4]),
                (whp::Register64::Rbp, state.gps[5]),
                (whp::Register64::Rsi, state.gps[6]),
                (whp::Register64::Rdi, state.gps[7]),
                (whp::Register64::R8, state.gps[8]),
                (whp::Register64::R9, state.gps[9]),
                (whp::Register64::R10, state.gps[10]),
                (whp::Register64::R11, state.gps[11]),
                (whp::Register64::R12, state.gps[12]),
                (whp::Register64::R13, state.gps[13]),
                (whp::Register64::R14, state.gps[14]),
                (whp::Register64::R15, state.gps[15]),
            ]
        )
        .unwrap()
    }

    pub(crate) fn translation_registers(&self, vtl: Vtl) -> TranslationRegisters {
        let (cr0, cr4, efer, cr3, rflags, ss) = whp::get_registers!(
            self.vp.whp(vtl),
            [
                whp::Register64::Cr0,
                whp::Register64::Cr4,
                whp::Register64::Efer,
                whp::Register64::Cr3,
                whp::Register64::Rflags,
                whp::RegisterSegment::Ss
            ]
        )
        .expect("register reads cannot fail");

        TranslationRegisters {
            cr0,
            cr4,
            efer,
            cr3,
            rflags,
            ss: from_seg_reg(&ss),
            encryption_mode: virt_support_x86emu::translate::EncryptionMode::None,
        }
    }
}
