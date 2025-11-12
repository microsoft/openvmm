// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! x86_64-specific implementation of Hyper-V test context implementation

use alloc::alloc::alloc;
use alloc::boxed::Box;
use core::alloc::Layout;
use core::arch::asm;
use core::ops::Range;
#[cfg(target_arch = "x86_64")] // xtask-fmt allow-target-arch sys-crate
use hvdef::hypercall::InitialVpContextX64;

use hvdef::AlignedU128;
use hvdef::HvRegisterValue;
use hvdef::HvX64RegisterName;
use hvdef::Vtl;
use hvdef::hypercall::HvInputVtl;
use memory_range::MemoryRange;
use minimal_rt::arch::msr::read_msr;
use minimal_rt::arch::msr::write_msr;

#[cfg(nightly)]
use crate::context::InterruptPlatformTrait;
use crate::context::MsrPlatformTrait;
#[cfg(nightly)]
use crate::context::SecureInterceptPlatformTrait;
use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VpExecToken;
use crate::context::VtlPlatformTrait;
use crate::platform::hyperv::arch::hypercall::HvCall;
use crate::platform::hyperv::ctx::HvTestCtx;
use crate::platform::hyperv::ctx::cmdt;
use crate::platform::hyperv::ctx::get_vp_set;
use crate::platform::hyperv::ctx::vtl_transform;
use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;

#[cfg(nightly)]
impl SecureInterceptPlatformTrait for HvTestCtx {
    /// Configure the Secure Interrupt Message Page (SIMP) and the first
    /// SynIC interrupt (SINT0) so that the hypervisor can vector
    /// hypervisor side notifications back to the guest.  
    fn setup_secure_intercept(&mut self, interrupt_idx: u8) -> TmkResult<()> {
        let layout = Layout::from_size_align(4096, 4096).map_err(|_| TmkError::AllocationFailed)?;

        // SAFETY: the pointer is managed carefully and is not deallocated until the end of the test.
        let ptr = unsafe { alloc(layout) };
        let gpn = (ptr as u64) >> 12;
        // toggle the enable bit of the SIMP register
        let reg = (gpn << 12) | 0x1;

        // SAFETY: we are writing to a valid MSR.
        unsafe { self.write_msr(hvdef::HV_X64_MSR_SIMP, reg)? };
        log::info!("Successfully set the SIMP register.");

        // SAFETY: we are writing to a valid MSR.
        let reg = unsafe { self.read_msr(hvdef::HV_X64_MSR_SINT0)? };
        let mut reg: hvdef::HvSynicSint = reg.into();
        reg.set_vector(interrupt_idx);
        reg.set_masked(false);
        reg.set_auto_eoi(true);

        // SAFETY: we are writing to a valid MSR.
        unsafe { self.write_msr(hvdef::HV_X64_MSR_SINT0, reg.into())? };
        log::info!("Successfully set the SINT0 register.");
        Ok(())
    }

    fn signal_intercept_handled(&mut self) -> TmkResult<()> {
        // SAFETY: we are reading from a valid MSR.
        let simp_page = unsafe { self.read_msr(hvdef::HV_X64_MSR_SIMP)? };

        if (simp_page & 0b1) == 0 {
            // return error if SIMP is not enabled
            return Err(TmkError::InvalidRegisterValue);
        }

        let simp_page_address = (simp_page & 0xFFFFFFFFFFFFF000) as *mut hvdef::HvMessage;

        // SAFETY: we are creating a mutable reference to a valid memory region
        // which is populated with valid data by a paravisor/hypervisor.
        let messages: &mut [hvdef::HvMessage] =
            unsafe { core::slice::from_raw_parts_mut(simp_page_address, 16) };

        // on hyper-v the hypervisor messages are received on SINT0
        messages[0].header.typ = hvdef::HvMessageType::HvMessageTypeNone;
        Ok(())
    }
}

#[cfg(nightly)]
impl InterruptPlatformTrait for HvTestCtx {
    /// Install an interrupt handler for the supplied vector on x86-64.
    fn set_interrupt_idx(&mut self, interrupt_idx: u8, handler: fn(HvTestCtx)) -> TmkResult<()> {
        let current_vtl = self.get_current_vtl()?;
        crate::arch::interrupt::set_handler(
            interrupt_idx,
            Box::new(move || {
                let mut ctx = HvTestCtx::new();
                _ = ctx.init(current_vtl);
                handler(ctx);
            }),
        );
        Ok(())
    }

    /// Initialise the minimal in-guest interrupt infrastructure
    fn setup_interrupt_handler(&mut self) -> TmkResult<()> {
        crate::arch::interrupt::init();
        Ok(())
    }
}

impl MsrPlatformTrait for HvTestCtx {
    /// Read an MSR directly from the CPU and return the raw value.
    unsafe fn read_msr(&mut self, msr: u32) -> TmkResult<u64> {
        // SAFETY: tests should only read to valid MSRs. Caller must ensure safety.
        let r = unsafe { read_msr(msr) };
        Ok(r)
    }

    /// Write an MSR directly on the CPU.
    unsafe fn write_msr(&mut self, msr: u32, value: u64) -> TmkResult<()> {
        // SAFETY: tests should only write to valid MSRs. Caller must ensure safety.
        unsafe { write_msr(msr, value) };
        Ok(())
    }
}

impl VirtualProcessorPlatformTrait<HvTestCtx> for HvTestCtx {
    /// Fetch the content of the specified architectural register from
    /// the current VTL for the executing VP.
    fn get_register(&mut self, reg: u32) -> TmkResult<u128> {
        let reg = HvX64RegisterName(reg);
        let val = self.hvcall.get_register(reg.into(), None)?.as_u128();
        Ok(val)
    }

    /// Set the architecture specific register identified by `reg`.
    fn set_register(&mut self, reg: u32, val: u128) -> TmkResult<()> {
        let reg = HvX64RegisterName(reg);
        let value = HvRegisterValue::from(val);
        self.hvcall.set_register(reg.into(), value, None)?;

        Ok(())
    }

    /// Return the number of logical processors present in the machine
    fn get_vp_count(&self) -> TmkResult<u32> {
        // TODO: use ACPI to get the actual count
        Ok(4)
    }

    /// Push a command onto the per-VP linked-list so it will be executed
    /// by the busy-loop running in `exec_handler`. No scheduling happens
    /// here – we simply enqueue.
    fn queue_command_vp(&mut self, cmd: VpExecToken<HvTestCtx>) -> TmkResult<()> {
        let (vp_index, vtl, cmd) = cmd.get();
        let cmd = cmd.ok_or(TmkError::QueueCommandFailed)?;
        cmdt()
            .lock()
            .get_mut(&vp_index)
            .unwrap()
            .push_back((cmd, vtl));
        Ok(())
    }

    #[inline(never)]
    /// Ensure the target VP is running in the requested VTL and queue
    /// the command for execution.  
    /// – If the VP is not yet running, it is started with a default
    ///   context.  
    /// – If the command targets a different VTL than the current one,
    ///   control is switched via `vtl_call` / `vtl_return` so that the
    ///   executor loop can pick the command up.  
    /// in short every VP acts as an executor engine and
    /// spins in `exec_handler` waiting for work.
    fn start_on_vp(&mut self, cmd: VpExecToken<HvTestCtx>) -> TmkResult<()> {
        let (vp_index, vtl, cmd) = cmd.get();
        let cmd = cmd.ok_or(TmkError::InvalidParameter)?;
        if vtl >= Vtl::Vtl2 {
            return Err(TmkError::InvalidParameter);
        }
        let is_vp_running = get_vp_set().lock().get(&vp_index).cloned();
        if let Some(_running_vtl) = is_vp_running {
            log::debug!("both vtl0 and vtl1 are running for VP: {:?}", vp_index);
        } else {
            if vp_index == 0 {
                let vp_context = self.get_default_context(Vtl::Vtl1)?;
                self.hvcall.enable_vp_vtl(0, Vtl::Vtl1, Some(vp_context))?;

                cmdt().lock().get_mut(&vp_index).unwrap().push_back((
                    Box::new(move |ctx| {
                        ctx.switch_to_low_vtl();
                    }),
                    Vtl::Vtl1,
                ));
                self.switch_to_high_vtl();
                get_vp_set().lock().insert(vp_index);
            } else {
                let (tx, rx) = nostd_spin_channel::Channel::<TmkResult<()>>::new().split();
                let self_vp_idx = self.my_vp_idx;
                cmdt().lock().get_mut(&self_vp_idx).unwrap().push_back((
                    Box::new(move |ctx| {
                        log::debug!("starting VP{} in VTL1 of vp{}", vp_index, self_vp_idx);
                        let r = ctx.enable_vp_vtl_with_default_context(vp_index, Vtl::Vtl1);
                        if r.is_err() {
                            log::error!("failed to enable VTL1 for VP{}: {:?}", vp_index, r);
                            let _ = tx.send(r);
                            return;
                        }
                        log::debug!("successfully enabled VTL1 for VP{}", vp_index);
                        let r = ctx.start_running_vp_with_default_context(VpExecToken::new(
                            vp_index,
                            Vtl::Vtl0,
                        ));
                        if r.is_err() {
                            log::error!("failed to start VP{}: {:?}", vp_index, r);
                            let _ = tx.send(r);
                            return;
                        }
                        log::debug!("successfully started VP{}", vp_index);
                        let _ = tx.send(Ok(()));
                        ctx.switch_to_low_vtl();
                    }),
                    Vtl::Vtl1,
                ));
                self.switch_to_high_vtl();
                let rx = rx.recv();
                if let Ok(r) = rx {
                    r?;
                }
                get_vp_set().lock().insert(vp_index);
            }
        }
        cmdt()
            .lock()
            .get_mut(&vp_index)
            .unwrap()
            .push_back((cmd, vtl));

        if vp_index == self.my_vp_idx && self.my_vtl != vtl {
            if vtl == Vtl::Vtl0 {
                self.switch_to_low_vtl();
            } else {
                self.switch_to_high_vtl();
            }
        }
        Ok(())
    }

    /// Start the given VP in the current VTL using a freshly captured
    /// context.
    fn start_running_vp_with_default_context(
        &mut self,
        cmd: VpExecToken<HvTestCtx>,
    ) -> TmkResult<()> {
        let (vp_index, vtl, _cmd) = cmd.get();
        let vp_ctx = self.get_default_context(vtl)?;
        self.hvcall
            .start_virtual_processor(vp_index, vtl, Some(vp_ctx))?;
        Ok(())
    }

    /// Return the index of the VP that is currently executing this code.
    fn get_current_vp(&self) -> TmkResult<u32> {
        Ok(self.my_vp_idx)
    }

    fn set_register_vtl(&mut self, reg: u32, value: u128, vtl: Vtl) -> TmkResult<()> {
        let reg = HvX64RegisterName(reg);
        let value = HvRegisterValue::from(value);
        self.hvcall
            .set_register(reg.into(), value, Some(vtl_transform(vtl)))?;

        Ok(())
    }

    fn get_register_vtl(&mut self, reg: u32, vtl: Vtl) -> TmkResult<u128> {
        let reg = HvX64RegisterName(reg);
        let val = self
            .hvcall
            .get_register(reg.into(), Some(vtl_transform(vtl)))?
            .as_u128();
        Ok(val)
    }
}

impl VtlPlatformTrait for HvTestCtx {
    /// Apply VTL protections to the supplied GPA range so that only the
    /// provided VTL can access it.
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl) -> TmkResult<()> {
        self.hvcall
            .apply_vtl_protections(MemoryRange::new(range), vtl)?;
        Ok(())
    }

    /// Enable the specified VTL on a VP and seed it with a default
    /// context captured from the current execution environment.
    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()> {
        let vp_ctx = self.get_default_context(vtl)?;
        self.hvcall.enable_vp_vtl(vp_index, vtl, Some(vp_ctx))?;
        Ok(())
    }

    /// Return the VTL in which the current code is running.
    fn get_current_vtl(&self) -> TmkResult<Vtl> {
        Ok(self.my_vtl)
    }

    /// Enable VTL support for the entire partition.
    fn setup_partition_vtl(&mut self, vtl: Vtl) -> TmkResult<()> {
        self.hvcall
            .enable_partition_vtl(hvdef::HV_PARTITION_ID_SELF, vtl)?;
        log::info!("enabled vtl protections for the partition.");
        Ok(())
    }

    /// Turn on VTL protections for the currently running VTL.
    fn setup_vtl_protection(&mut self) -> TmkResult<()> {
        self.hvcall.enable_vtl_protection(HvInputVtl::CURRENT_VTL)?;
        log::info!("enabled vtl protections for the partition.");
        Ok(())
    }

    /// Switch execution from the current (low) VTL to the next higher
    /// one (`vtl_call`).
    #[inline(never)]
    fn switch_to_high_vtl(&mut self) {
        // SAFETY: we are calling a valid function that switches to high VTL. With valid instructions
        // to save restore register states.
        unsafe {
            asm!(
                "
                push rax
                push rbx
                push rcx
                push rdx
                push rdi
                push rsi
                push rbp
                push r8
                push r9
                push r10
                push r11
                push r12
                push r13
                push r14
                push r15
                call {call_address}
                pop r15
                pop r14
                pop r13
                pop r12
                pop r11
                pop r10
                pop r9
                pop r8
                pop rbp
                pop rsi
                pop rdi
                pop rdx
                pop rcx
                pop rbx
                pop rax",
                call_address = sym HvCall::vtl_call,
            );
        }
    }

    /// Return from a high VTL back to the low VTL (`vtl_return`).
    #[inline(never)]
    fn switch_to_low_vtl(&mut self) {
        // SAFETY: we are calling a valid function that switches to low VTL. With valid instructions
        // to save restore register states.
        unsafe {
            asm!(
                "
                push rax
                push rbx
                push rcx
                push rdx
                push rdi
                push rsi
                push rbp
                push r8
                push r9
                push r10
                push r11
                push r12
                push r13
                push r14
                push r15
                call {call_address}
                pop r15
                pop r14
                pop r13
                pop r12
                pop r11
                pop r10
                pop r9
                pop r8
                pop rbp
                pop rsi
                pop rdi
                pop rdx
                pop rcx
                pop rbx
                pop rax",
                call_address = sym HvCall::vtl_return,
            );
        }
    }

    // Set the state of a virtual processor (VP) with the specified VTL.
    fn set_vp_register_with_vtl(
        &mut self,
        register_index: u32,
        value: u64,
        vtl: Vtl,
    ) -> TmkResult<()> {
        let vtl = vtl_transform(vtl);
        let value = AlignedU128::from(value);
        let reg_value = HvRegisterValue(value);
        self.hvcall
            .set_register(hvdef::HvRegisterName(register_index), reg_value, Some(vtl))
            .map_err(|e| e.into())
    }

    fn get_vp_register_with_vtl(&mut self, register_index: u32, vtl: Vtl) -> TmkResult<u64> {
        let vtl = vtl_transform(vtl);
        self.hvcall
            .get_register(hvdef::HvRegisterName(register_index), Some(vtl))
            .map(|v| v.as_u64())
            .map_err(|e| e.into())
    }
}

impl HvTestCtx {
    /// Return the index of the VP that is currently executing this code.
    pub(crate) fn get_vp_idx() -> u32 {
        // SAFETY: we are executing a valid CPUID instruction.
        let result = unsafe { core::arch::x86_64::__cpuid(0x1) };
        (result.ebx >> 24) & 0xFF
    }

    /// Capture the current VP context, patch the entry point and stack
    /// so that the new VP starts in `exec_handler`.
    pub(crate) fn get_default_context(
        &mut self,
        vtl: Vtl,
    ) -> Result<InitialVpContextX64, TmkError> {
        let handler = match vtl {
            Vtl::Vtl0 => HvTestCtx::general_exec_handler,
            Vtl::Vtl1 => HvTestCtx::secure_exec_handler,
            _ => return Err(TmkError::InvalidParameter),
        };
        self.exec_fn_with_current_context(handler)
    }

    /// Helper to return an arbitrary function with a captured VP context
    /// that can later be used to start a new VP/VTL instance.
    fn exec_fn_with_current_context(
        &mut self,
        func: fn(),
    ) -> Result<InitialVpContextX64, TmkError> {
        let mut vp_context: InitialVpContextX64 = self
            .hvcall
            .get_current_vtl_vp_context()
            .expect("Failed to get current VTL context");
        let stack_layout = Layout::from_size_align(1024 * 1024, 16)
            .expect("Failed to create layout for stack allocation");
        // SAFETY: the pointer is managed carefully and is not deallocated until the end of the test.
        let allocated_stack_ptr = unsafe { alloc(stack_layout) };
        if allocated_stack_ptr.is_null() {
            return Err(TmkError::AllocationFailed);
        }
        let stack_size = stack_layout.size();
        let stack_top = allocated_stack_ptr as u64 + stack_size as u64;
        let fn_address = func as usize as u64;
        vp_context.rip = fn_address;
        vp_context.rsp = stack_top;
        Ok(vp_context)
    }
}
