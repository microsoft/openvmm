// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sidecar code that runs on the APs, after initialization. This code all runs
//! with per-AP page tables, concurrently with the main kernel.

use super::CommandErrorWriter;
use super::VSM_CAPABILITIES;
use super::VTL_RETURN_OFFSET;
use super::VpGlobals;
use super::addr_space;
use super::get_hv_vp_register;
use super::hypercall;
use super::log;
use super::set_hv_vp_register;
use core::fmt::Write;
use core::mem::size_of;
use core::ptr::addr_of;
use core::sync::atomic::AtomicU8;
use core::sync::atomic::Ordering::Acquire;
use core::sync::atomic::Ordering::Relaxed;
use core::sync::atomic::Ordering::Release;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HV_PARTITION_ID_SELF;
use hvdef::HV_VP_INDEX_SELF;
use hvdef::HvStatus;
use hvdef::HvVtlEntryReason;
use hvdef::HvX64RegisterName;
use hvdef::HypercallCode;
use hvdef::hypercall::HvInputVtl;
use hvdef::hypercall::HvRegisterAssoc;
use hvdef::hypercall::TranslateVirtualAddressX64;
use minimal_rt::arch::hypercall::HYPERCALL_PAGE;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;
use sidecar_defs::CommandPage;
use sidecar_defs::ControlPage;
use sidecar_defs::CpuContextX64;
use sidecar_defs::CpuStatus;
use sidecar_defs::GetSetVpRegisterRequest;
use sidecar_defs::PAGE_SIZE;
use sidecar_defs::RunVpResponse;
use sidecar_defs::SidecarCommand;
use sidecar_defs::TranslateGvaRequest;
use sidecar_defs::TranslateGvaResponse;
use x86defs::apic::ApicBase;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Entry point for an AP. Called with per-VP state (page tables, stack,
/// globals) already initialized, and IDT and GDT set appropriately.
///
/// # Safety
/// Must be called as an AP entry point.
pub unsafe fn ap_entry() -> ! {
    // SAFETY: the globals are only accessed by this CPU, and so there are no
    // concurrent accesses.
    let globals = unsafe { &mut *addr_space::globals() };

    // Set fs base to point to the CPU context, for use in `run_vp`.
    //
    // SAFETY: just getting the address.
    let fs_base = unsafe { addr_of!((*addr_space::command_page()).cpu_context) as u64 };
    // SAFETY: no safety requirements.
    unsafe { write_msr(x86defs::X64_MSR_FS_BASE, fs_base) };

    // Enable the X2APIC.
    //
    // SAFETY: no safety requirements.
    let apic_base = ApicBase::from(unsafe { read_msr(x86defs::X86X_MSR_APIC_BASE) });
    // SAFETY: no safety requirements.
    unsafe {
        write_msr(
            x86defs::X86X_MSR_APIC_BASE,
            apic_base.with_enable(true).with_x2apic(true).into(),
        )
    }

    // Software enable the APIC.
    //
    // SAFETY: the IDT is initialized appropriately.
    unsafe {
        write_msr(
            x86defs::apic::ApicRegister::SVR.x2apic_msr(),
            u32::from(x86defs::apic::Svr::new().with_enable(true).with_vector(!0)).into(),
        )
    }

    // Zero the register page, since it has not yet been mapped (and may never
    // be mapped if the hypervisor does not support it).
    //
    // Note that it is not safe to access the register page after this point,
    // since it is owned by the VMM.
    //
    // SAFETY: we are still booting, so the VMM is not using this yet.
    unsafe {
        (*addr_space::register_page().cast::<[u8; PAGE_SIZE]>()).fill(0);
    }

    // Zero the command page, too, so that it doesn't leak data to the VMM from
    // a previous boot.
    //
    // SAFETY: we are still booting, so the VMM is not using this yet.
    unsafe {
        (*addr_space::command_page().cast::<[u8; PAGE_SIZE]>()).fill(0);
    }

    // Notify the BSP that we are ready.
    let old_state = globals.cpu_status().swap(CpuStatus::IDLE.0, Release);
    assert_eq!(old_state, CpuStatus::RUN.0);

    // Run the AP command dispatch loop until we receive a remove request.
    ap_run(globals);

    log!("removing");

    // Disable the VP assist page.
    //
    // SAFETY: no safety requirements.
    unsafe { write_msr(hvdef::HV_X64_MSR_VP_ASSIST_PAGE, 0) };

    // Disable the register page.
    if globals.register_page_mapped {
        set_hv_vp_register(
            HvInputVtl::CURRENT_VTL,
            HvX64RegisterName::RegisterPage.into(),
            0u64.into(),
        )
        .unwrap();
    }

    // Software disable the APIC. Leave the hardware enabled so that we can send
    // the response IPI.
    log!("disabling apic");
    // SAFETY: no safety requirements.
    unsafe {
        write_msr(
            x86defs::apic::ApicRegister::SVR.x2apic_msr(),
            u32::from(x86defs::apic::Svr::new().with_enable(false).with_vector(!0)).into(),
        );
    }
    globals.cpu_status().store(CpuStatus::REMOVED.0, Release);
    raise_attention();
    park_until(|| None)
}

fn map_overlays(globals: &mut VpGlobals) {
    // Enable the VP assist page.
    //
    // SAFETY: the VP assist page is reserved for this use and will not alias
    // with other Rust memory.
    unsafe {
        write_msr(
            hvdef::HV_X64_MSR_VP_ASSIST_PAGE,
            hvdef::HvRegisterVpAssistPage::new()
                .with_enabled(true)
                .with_gpa_page_number(addr_space::assist_page_pa() >> HV_PAGE_SHIFT)
                .into(),
        );
    }

    // Map the register page. We don't currently use it directly, but it is
    // provided to the VMM.
    match set_hv_vp_register(
        HvInputVtl::CURRENT_VTL,
        HvX64RegisterName::RegisterPage.into(),
        u64::from(
            hvdef::HvSynicSimpSiefp::new()
                .with_base_gpn(addr_space::register_page_pa() >> HV_PAGE_SHIFT)
                .with_enabled(true),
        )
        .into(),
    ) {
        Ok(()) => globals.register_page_mapped = true,
        Err(err) => {
            // This may be an expected condition if the hypervisor does not support
            // the register page for VTL2.
            log!("failed to map register page: {err}");
        }
    }
}

/// Runs the command dispatch loop for an AP until a remove request is received.
fn ap_run(globals: &mut VpGlobals) {
    let cpu_status = globals.cpu_status();

    loop {
        // Wait for a run request.
        let status = park_until(|| {
            let status = CpuStatus(cpu_status.load(Acquire));
            (status != CpuStatus::IDLE).then_some(status)
        });
        match status {
            CpuStatus::RUN | CpuStatus::STOP => {
                // Still run the request if a stop is requested, since there
                // is no generic way to report that the request was
                // cancelled before it ran.
            }
            CpuStatus::REMOVE => return,
            status => panic!("unexpected cpu request {status:?}"),
        }

        // Dispatch on the command page.
        {
            // SAFETY: we now have exclusive access to the state.
            let command_page = unsafe { &mut *addr_space::command_page() };
            command_page.has_error = 0;
            let command = core::mem::replace(&mut command_page.command, SidecarCommand::NONE);
            log!("request {command:?}");
            match command {
                SidecarCommand::NONE => {}
                SidecarCommand::RUN_VP => run_vp(globals, command_page, cpu_status),
                SidecarCommand::GET_VP_REGISTERS => get_vp_registers(command_page),
                SidecarCommand::SET_VP_REGISTERS => set_vp_registers(command_page),
                SidecarCommand::TRANSLATE_GVA => translate_gva(command_page),
                command => set_error(command_page, format_args!("unknown command {command:?}")),
            }
        };

        log!("request done");
        cpu_status.store(CpuStatus::IDLE.0, Release);
        raise_attention();
    }
}

/// Returns a reference to the control page for this VP.
/// Only use this on AP startup paths.
pub fn control() -> &'static ControlPage {
    // SAFETY: all mutable fields of the control page have interior mutability,
    // so this is a valid dereference.
    unsafe { &*addr_space::control_page() }
}

impl VpGlobals {
    fn cpu_status(&self) -> &'static AtomicU8 {
        &control().cpu_status[self.node_cpu_index as usize]
    }
}

fn set_error(command_page: &mut CommandPage, err: impl core::fmt::Display) {
    command_page.has_error = 1;
    command_page.error.len = 0;
    let mut writer = CommandErrorWriter(&mut command_page.error);
    let _ = write!(writer, "{err}");
}

fn run_vp(globals: &mut VpGlobals, command_page: &mut CommandPage, cpu_status: &AtomicU8) {
    // Map the register page and VP assist page now.
    //
    // The hypervisor has a concurrency bug if the pages are mapped while other
    // VPs are starting up, so work around this by delaying it until now.
    //
    // The VP assist page is only needed in this path. The register page is
    // technically used by the user-mode VMM earlier, but the hypervisor doesn't
    // mark it valid until the first time the VP is run anyway.
    if !globals.overlays_mapped {
        map_overlays(globals);
        globals.overlays_mapped = true;
    }

    let mut intercept = false;
    while cpu_status.load(Relaxed) != CpuStatus::STOP.0 {
        match run_vp_once(command_page) {
            Ok(true) => {
                intercept = true;
                break;
            }
            Ok(false) => {}
            Err(()) => return,
        }
    }

    RunVpResponse {
        intercept: intercept as u8,
    }
    .write_to_prefix(command_page.request_data.as_mut_bytes())
    .unwrap(); // PANIC: will not panic, since sizeof(RunVpResponse) is 1, whereas the buffer is statically declared as 16 bytes long.
}

fn run_vp_once(command_page: &mut CommandPage) -> Result<bool, ()> {
    let cpu_context = &mut command_page.cpu_context;
    // Write rax and rcx to the VP assist page.
    //
    // SAFETY: the assist page is not concurrently modified.
    unsafe {
        (*addr_space::assist_page()).vtl_control.registers = [
            cpu_context.gps[CpuContextX64::RAX],
            cpu_context.gps[CpuContextX64::RCX],
        ];
    }
    // Dispatch the VP.
    //
    // SAFETY: no safety requirements for this hypercall.
    unsafe {
        core::arch::asm! {
            "push rbp",
            "push rbx",
            "mov rbp, fs:[0x28]",
            "mov rbx, fs:[0x18]",
            "call rax",
            "mov fs:[0x18], rbx",
            "mov fs:[0x28], rbp",
            "mov rbx, cr2",
            "mov fs:[0x20], rbx",
            "pop rbx",
            "pop rbp",
            "fxsave fs:[0x80]",
            in("rax") addr_of!(HYPERCALL_PAGE) as usize + *addr_of!(VTL_RETURN_OFFSET) as usize,
            lateout("rax") cpu_context.gps[CpuContextX64::RAX],
            inout("rcx") 0u64 => cpu_context.gps[CpuContextX64::RCX], // normal return
            inout("rdx") cpu_context.gps[CpuContextX64::RDX],
            inout("rsi") cpu_context.gps[CpuContextX64::RSI],
            inout("rdi") cpu_context.gps[CpuContextX64::RDI],
            inout("r8") cpu_context.gps[CpuContextX64::R8],
            inout("r9") cpu_context.gps[CpuContextX64::R9],
            inout("r10") cpu_context.gps[CpuContextX64::R10],
            inout("r11") cpu_context.gps[CpuContextX64::R11],
            inout("r12") cpu_context.gps[CpuContextX64::R12],
            inout("r13") cpu_context.gps[CpuContextX64::R13],
            inout("r14") cpu_context.gps[CpuContextX64::R14],
            inout("r15") cpu_context.gps[CpuContextX64::R15],
        }
    }
    // SAFETY: the assist page is not concurrently modified.
    let entry_reason = unsafe { (*addr_space::assist_page()).vtl_control.entry_reason };
    match entry_reason {
        HvVtlEntryReason::INTERRUPT => Ok(false),
        HvVtlEntryReason::INTERCEPT => {
            // SAFETY: the assist page is not concurrently modified.
            let intercept_message =
                unsafe { &*addr_of!((*addr_space::assist_page()).intercept_message) };
            command_page.intercept_message = *intercept_message;
            Ok(true)
        }
        entry_reason => {
            set_error(
                command_page,
                format_args!("unexpected entry reason {entry_reason:?}"),
            );
            Err(())
        }
    }
}

fn shared_msr(name: HvX64RegisterName) -> Option<u32> {
    let msr = match name {
        HvX64RegisterName::MsrMtrrDefType => x86defs::X86X_MSR_MTRR_DEF_TYPE,
        HvX64RegisterName::MsrMtrrFix64k00000 => x86defs::X86X_MSR_MTRR_FIX64K_00000,
        HvX64RegisterName::MsrMtrrFix16k80000 => x86defs::X86X_MSR_MTRR_FIX16K_80000,
        HvX64RegisterName::MsrMtrrFix16kA0000 => x86defs::X86X_MSR_MTRR_FIX16K_A0000,
        HvX64RegisterName::MsrMtrrFix4kC0000 => x86defs::X86X_MSR_MTRR_FIX4K_C0000,
        HvX64RegisterName::MsrMtrrFix4kC8000 => x86defs::X86X_MSR_MTRR_FIX4K_C8000,
        HvX64RegisterName::MsrMtrrFix4kD0000 => x86defs::X86X_MSR_MTRR_FIX4K_D0000,
        HvX64RegisterName::MsrMtrrFix4kD8000 => x86defs::X86X_MSR_MTRR_FIX4K_D8000,
        HvX64RegisterName::MsrMtrrFix4kE0000 => x86defs::X86X_MSR_MTRR_FIX4K_E0000,
        HvX64RegisterName::MsrMtrrFix4kE8000 => x86defs::X86X_MSR_MTRR_FIX4K_E8000,
        HvX64RegisterName::MsrMtrrFix4kF0000 => x86defs::X86X_MSR_MTRR_FIX4K_F0000,
        HvX64RegisterName::MsrMtrrFix4kF8000 => x86defs::X86X_MSR_MTRR_FIX4K_F8000,
        HvX64RegisterName::MsrMtrrPhysBase0 => x86defs::X86X_MSR_MTRR_PHYSBASE0,
        HvX64RegisterName::MsrMtrrPhysMask0 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 1,
        HvX64RegisterName::MsrMtrrPhysBase1 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 2,
        HvX64RegisterName::MsrMtrrPhysMask1 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 3,
        HvX64RegisterName::MsrMtrrPhysBase2 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 4,
        HvX64RegisterName::MsrMtrrPhysMask2 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 5,
        HvX64RegisterName::MsrMtrrPhysBase3 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 6,
        HvX64RegisterName::MsrMtrrPhysMask3 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 7,
        HvX64RegisterName::MsrMtrrPhysBase4 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 8,
        HvX64RegisterName::MsrMtrrPhysMask4 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 9,
        HvX64RegisterName::MsrMtrrPhysBase5 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 10,
        HvX64RegisterName::MsrMtrrPhysMask5 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 11,
        HvX64RegisterName::MsrMtrrPhysBase6 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 12,
        HvX64RegisterName::MsrMtrrPhysMask6 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 13,
        HvX64RegisterName::MsrMtrrPhysBase7 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 14,
        HvX64RegisterName::MsrMtrrPhysMask7 => x86defs::X86X_MSR_MTRR_PHYSBASE0 + 15,
        _ => return None,
    };
    Some(msr)
}

fn set_debug_register(name: HvX64RegisterName, value: u64) -> bool {
    // SAFETY: debug registers are unused by sidecar.
    unsafe {
        match name {
            HvX64RegisterName::Dr0 => core::arch::asm!("mov dr0, {}", in(reg) value),
            HvX64RegisterName::Dr1 => core::arch::asm!("mov dr1, {}", in(reg) value),
            HvX64RegisterName::Dr2 => core::arch::asm!("mov dr2, {}", in(reg) value),
            HvX64RegisterName::Dr3 => core::arch::asm!("mov dr3, {}", in(reg) value),
            HvX64RegisterName::Dr6 if (&raw const VSM_CAPABILITIES).read().dr6_shared() => {
                core::arch::asm!("mov dr6, {}", in(reg) value)
            }
            _ => return false,
        }
    }

    true
}

fn get_debug_register(name: HvX64RegisterName) -> Option<u64> {
    let v: u64;
    // SAFETY: debug registers are unused by sidecar.
    unsafe {
        match name {
            HvX64RegisterName::Dr0 => core::arch::asm!("mov {}, dr0", lateout(reg) v),
            HvX64RegisterName::Dr1 => core::arch::asm!("mov {}, dr1", lateout(reg) v),
            HvX64RegisterName::Dr2 => core::arch::asm!("mov {}, dr2", lateout(reg) v),
            HvX64RegisterName::Dr3 => core::arch::asm!("mov {}, dr3", lateout(reg) v),
            HvX64RegisterName::Dr6 if (&raw const VSM_CAPABILITIES).read().dr6_shared() => {
                core::arch::asm!("mov {}, dr6", lateout(reg) v)
            }
            _ => return None,
        }
    }
    Some(v)
}

fn get_vp_registers(command_page: &mut CommandPage) {
    let (request, regs) = command_page
        .request_data
        .as_mut_bytes()
        .split_at_mut(size_of::<GetSetVpRegisterRequest>());
    let &mut GetSetVpRegisterRequest {
        count,
        target_vtl,
        rsvd: _,
        ref mut status,
        rsvd2: _,
        regs: [],
    } = FromBytes::mut_from_bytes(request).unwrap();

    let Ok((regs, _)) = <[HvRegisterAssoc]>::mut_from_prefix_with_elems(regs, count.into()) else {
        // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
        set_error(
            command_page,
            format_args!("invalid register name count: {count}"),
        );
        return;
    };

    *status = HvStatus::SUCCESS;
    for &mut HvRegisterAssoc {
        name,
        pad: _,
        ref mut value,
    } in regs
    {
        let r = if let Some(msr) = shared_msr(name.into()) {
            // SAFETY: the shared MSRs are not used by this kernel, so they cannot
            // affect this kernel's functioning.
            Ok(unsafe { read_msr(msr).into() })
        } else if let Some(value) = get_debug_register(name.into()) {
            Ok(value.into())
        } else {
            // FUTURE: consider batching these hypercalls if this becomes a bottleneck.
            get_hv_vp_register(target_vtl, name)
        };

        match r {
            Ok(v) => *value = v,
            Err(err) => {
                *status = Err(err).into();
                break;
            }
        };
    }
}

fn set_vp_registers(command_page: &mut CommandPage) {
    let (request, regs) = command_page
        .request_data
        .as_mut_bytes()
        .split_at_mut(size_of::<GetSetVpRegisterRequest>());
    let &mut GetSetVpRegisterRequest {
        count,
        target_vtl,
        rsvd: _,
        ref mut status,
        rsvd2: _,
        regs: [],
    } = FromBytes::mut_from_bytes(request).unwrap();

    let Ok((assoc, _)) = <[HvRegisterAssoc]>::ref_from_prefix_with_elems(regs, count.into()) else {
        // TODO: zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
        set_error(
            command_page,
            format_args!("invalid register count: {count}"),
        );
        return;
    };

    *status = HvStatus::SUCCESS;
    for &HvRegisterAssoc {
        name,
        value,
        pad: _,
    } in assoc
    {
        let r = if let Some(msr) = shared_msr(name.into()) {
            // SAFETY: the shared MSRs are not used by this kernel, so they cannot
            // affect this kernel's functioning.
            unsafe { write_msr(msr, value.as_u64()) }
            Ok(())
        } else if set_debug_register(name.into(), value.as_u64()) {
            Ok(())
        } else {
            // FUTURE: consider batching these hypercalls if this becomes a bottleneck.
            set_hv_vp_register(target_vtl, name, value)
        };

        if r.is_err() {
            *status = r.into();
            break;
        }
    }
}

fn translate_gva(command_page: &mut CommandPage) {
    let TranslateGvaRequest { gvn, control_flags } =
        FromBytes::read_from_prefix(command_page.request_data.as_bytes())
            .unwrap()
            .0; // TODO: zerocopy: use-rest-of-range, zerocopy: err (https://github.com/microsoft/openvmm/issues/759)
    {
        // SAFETY: the input page is not concurrently accessed.
        let input = unsafe { &mut *addr_space::hypercall_input() };

        TranslateVirtualAddressX64 {
            partition_id: HV_PARTITION_ID_SELF,
            vp_index: HV_VP_INDEX_SELF,
            reserved: 0,
            control_flags,
            gva_page: gvn,
        }
        .write_to_prefix(input)
        .unwrap();
    }

    let result = hypercall(HypercallCode::HvCallTranslateVirtualAddressEx, 0);
    let output = if result.is_ok() {
        // SAFETY: the output is not concurrently accessed
        let output = unsafe { &*addr_space::hypercall_output() };
        FromBytes::read_from_prefix(output).unwrap().0 // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
    } else {
        FromZeros::new_zeroed()
    };

    TranslateGvaResponse {
        status: result.into(),
        rsvd: [0; 7],
        output,
    }
    .write_to_prefix(command_page.request_data.as_mut_bytes())
    .unwrap();
}

fn raise_attention() {
    let control = control();
    control.needs_attention.store(1, Release);
    let vector = control.response_vector.load(Relaxed);
    if vector != 0 {
        log!("ipi vector {vector}");
        // SAFETY: no safety requirements.
        unsafe {
            write_msr(
                x86defs::apic::ApicRegister::ICR0.x2apic_msr(),
                x86defs::apic::Icr::new()
                    .with_x2apic_mda(control.response_cpu.load(Relaxed))
                    .with_vector(vector as u8)
                    .into(),
            );
        }
    }
}

fn park_until<F: FnMut() -> Option<R>, R>(mut f: F) -> R {
    loop {
        if let Some(r) = f() {
            break r;
        } else {
            // Enable interrupts and halt the processor. Disable interrupts
            // after waking up.
            //
            // SAFETY: no safety requirements.
            unsafe {
                core::arch::asm!("sti; hlt; cli");
            }
        }
    }
}
