#![cfg_attr(target_arch = "aarch64", expect(unused_imports))]
use alloc::alloc::alloc;
use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
use alloc::collections::btree_set::BTreeSet;
use alloc::collections::linked_list::LinkedList;
#[cfg(target_arch = "aarch64")]
use hvdef::hypercall::InitialVpContextArm64;
use core::alloc::Layout;
use core::arch::asm;
use core::fmt::Display;
use core::ops::Range;

use hvdef::hypercall::HvInputVtl;
#[cfg(target_arch = "x86_64")]
use hvdef::hypercall::InitialVpContextX64;
use hvdef::AlignedU128;
use hvdef::Vtl;
use memory_range::MemoryRange;
#[cfg(target_arch = "x86_64")]
use minimal_rt::arch::msr::read_msr;
#[cfg(target_arch = "x86_64")]
use minimal_rt::arch::msr::write_msr;
use spin::Mutex;

#[cfg(feature = "nightly")]
#[cfg(target_arch = "x86_64")]
use crate::context::InterruptPlatformTrait;
#[cfg(target_arch = "x86_64")]
use crate::context::MsrPlatformTrait;
#[cfg(feature = "nightly")]
#[cfg(target_arch = "x86_64")]
use crate::context::SecureInterceptPlatformTrait;
use crate::context::VirtualProcessorPlatformTrait;
use crate::context::VpExecutor;
use crate::context::VtlPlatformTrait;
use crate::hypercall::HvCall;
use crate::tmkdefs::TmkError;
use crate::tmkdefs::TmkResult;

type CommandTable = BTreeMap<u32, LinkedList<(Box<dyn FnOnce(&mut HvTestCtx) + 'static>, Vtl)>>;
static mut CMD: Mutex<CommandTable> = Mutex::new(BTreeMap::new());

#[expect(static_mut_refs)]
fn cmdt() -> &'static Mutex<CommandTable> {
    unsafe { &CMD }
}

fn register_command_queue(vp_index: u32) {
    log::debug!("registering command queue for vp: {}", vp_index);
    if cmdt().lock().get(&vp_index).is_none() {
        cmdt().lock().insert(vp_index, LinkedList::new());
        log::debug!("registered command queue for vp: {}", vp_index);
    } else {
        log::debug!("command queue already registered for vp: {}", vp_index);
    }
}

pub struct HvTestCtx {
    pub hvcall: HvCall,
    // TODO: make this static, this could lead to bugs when init a VP from AP
    pub vp_running: BTreeSet<u32>,
    pub my_vp_idx: u32,
    pub my_vtl: Vtl,
}

impl Display for HvTestCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "HvTestCtx {{ vp_idx: {}, vtl: {:?} }}",
            self.my_vp_idx, self.my_vtl
        )
    }
}

#[cfg(feature = "nightly")]
#[cfg(target_arch = "x86_64")]
impl SecureInterceptPlatformTrait for HvTestCtx {
    /// Configure the Secure Interrupt Message Page (SIMP) and the first
    /// SynIC interrupt (SINT0) so that the hypervisor can vector
    /// hypervisor side notifications back to the guest.  
    /// Returns [`TmkError`] if the allocation of the SIMP buffer fails.
    fn setup_secure_intercept(&mut self, interrupt_idx: u8) -> TmkResult<()> {
        let layout = Layout::from_size_align(4096, 4096)
            .map_err(|_| TmkError::AllocationFailed)?;

        let ptr = unsafe { alloc(layout) };
        let gpn = (ptr as u64) >> 12;
        let reg = (gpn << 12) | 0x1;

        self.write_msr(hvdef::HV_X64_MSR_SIMP, reg)?;
        log::info!("Successfuly set the SIMP register.");

        let reg = self.read_msr(hvdef::HV_X64_MSR_SINT0)?;
        let mut reg: hvdef::HvSynicSint = reg.into();
        reg.set_vector(interrupt_idx);
        reg.set_masked(false);
        reg.set_auto_eoi(true);

        self.write_msr(hvdef::HV_X64_MSR_SINT0, reg.into())?;
        log::info!("Successfuly set the SINT0 register.");
        Ok(())
    }
}

#[cfg(feature = "nightly")]
#[cfg(target_arch = "x86_64")]
impl InterruptPlatformTrait for HvTestCtx {
    /// Install an interrupt handler for the supplied vector on x86-64.
    /// For non-x86-64 targets the call returns
    /// [`TmkError::NotImplemented`].
    fn set_interrupt_idx(&mut self, interrupt_idx: u8, handler: fn()) -> TmkResult<()> {
        #[cfg(target_arch = "x86_64")]
        {
            crate::arch::interrupt::set_handler(interrupt_idx, handler);
            Ok(())
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(TmkError::NotImplemented)
        }
    }

    /// Initialise the minimal in-guest interrupt infrastructure
    /// (IDT/GIC, etc. depending on architecture).
    fn setup_interrupt_handler(&mut self) -> TmkResult<()> {
        crate::arch::interrupt::init();
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
impl MsrPlatformTrait for HvTestCtx {
    /// Read an MSR directly from the CPU and return the raw value.
    fn read_msr(&mut self, msr: u32) -> TmkResult<u64> {
        let r = unsafe { read_msr(msr) };
        Ok(r)
    }

    /// Write an MSR directly on the CPU.
    fn write_msr(&mut self, msr: u32, value: u64) -> TmkResult<()> {
        unsafe { write_msr(msr, value) };
        Ok(())
    }
}

impl VirtualProcessorPlatformTrait<HvTestCtx> for HvTestCtx {
    /// Fetch the content of the specified architectural register from
    /// the current VTL for the executing VP.
    fn get_register(&mut self, reg: u32) -> TmkResult<u128> {
        #[cfg(target_arch = "x86_64")]
        {
            use hvdef::HvX64RegisterName;
            let reg = HvX64RegisterName(reg);
            let val = self.hvcall.get_register(reg.into(), None)?.as_u128();
            Ok(val)
        }

        #[cfg(target_arch = "aarch64")]
        {
            let reg = hvdef::HvArm64RegisterName(reg);
            let val = self.hvcall.get_register(reg.into(), None)?.as_u128();
            Ok(val)
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Err(TmkError(TmkError::NotImplemented))
        }
    }

    /// Return the number of logical processors present in the machine
    /// by issuing the `cpuid` leaf 1 call on x86-64.
    fn get_vp_count(&self) -> TmkResult<u32> {
        #[cfg(target_arch = "x86_64")]
        {
            Ok(4)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(TmkError::NotImplemented)
        }
    }

    /// Push a command onto the per-VP linked-list so it will be executed
    /// by the busy-loop running in `exec_handler`. No scheduling happens
    /// here – we simply enqueue.
    fn queue_command_vp(&mut self, cmd: VpExecutor<HvTestCtx>) -> TmkResult<()> {
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
    fn start_on_vp(&mut self, cmd: VpExecutor<HvTestCtx>) -> TmkResult<()> {
        let (vp_index, vtl, cmd) = cmd.get();
        let cmd = cmd.ok_or(TmkError::InvalidParameter)?;
        if vtl >= Vtl::Vtl2 {
            return Err(TmkError::InvalidParameter);
        }
        let is_vp_running = self.vp_running.get(&vp_index);
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
                log::info!("self addr: {:p}", self as *const _);
                self.switch_to_high_vtl();
                log::info!("self addr after switch: {:p}", self as *const _);
                self.vp_running.insert(vp_index);
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
                        let r = ctx.start_running_vp_with_default_context(VpExecutor::new(
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
                self.vp_running.insert(vp_index);
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
    /// context and *do not* queue any additional work.
    fn start_running_vp_with_default_context(
        &mut self,
        cmd: VpExecutor<HvTestCtx>,
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

    /// Inject a default context into an already existing VP/VTL pair.
    fn set_default_ctx_to_vp(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()> {
        let i: u8 = match vtl {
            Vtl::Vtl0 => 0,
            Vtl::Vtl1 => 1,
            Vtl::Vtl2 => 2,
        };
        let vp_context = self.get_default_context(vtl)?;
        self.hvcall.set_vp_registers(
            vp_index,
            Some(
                HvInputVtl::new()
                    .with_target_vtl_value(i)
                    .with_use_target_vtl(true),
            ),
            Some(vp_context),
        )?;
        Ok(())
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

        // let reg = self
        //     .get_register(hvdef::HvAllArchRegisterName::VsmCodePageOffsets.0)
        //     .unwrap();
        // let reg = HvRegisterValue::from(reg);
        // let offset = hvdef::HvRegisterVsmCodePageOffsets::from_bits(reg.as_u64());

        // log::debug!("call_offset: {:?}", offset);

        // let call_offset = offset.call_offset();
        // unsafe {
        //     let call_address = &raw const HYPERCALL_PAGE as *const u8;
        //     let off_addr = call_address.add(call_offset.into()) as u64;
        //     asm!(
        //         "
        //         call {call_address}",
        //         in("rcx") 0x0,
        //         call_address = in(reg) off_addr,
        //     );
        // }
    }

    /// Return from a high VTL back to the low VTL (`vtl_return`).
    #[inline(never)]
    fn switch_to_low_vtl(&mut self) {
        // HvCall::vtl_return();
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
        // let reg = self
        //     .get_register(hvdef::HvAllArchRegisterName::VsmCodePageOffsets.0)
        //     .unwrap();
        // let reg = HvRegisterValue::from(reg);
        // let offset = hvdef::HvRegisterVsmCodePageOffsets::from_bits(reg.as_u64());

        // let call_offset = offset.return_offset();
        // unsafe {
        //     let call_address = &raw const HYPERCALL_PAGE as *const u8;
        //     let off_addr = call_address.add(call_offset.into()) as u64;
        //     asm!(
        //         "
        //         call {call_address}",
        //         in("rcx") 0x0,
        //         call_address = in(reg) off_addr,
        //     );
        // }
    }

    fn set_vp_state_with_vtl(
        &mut self,
        register_index: u32,
        value: u64,
        vtl: Vtl,
    ) -> TmkResult<()> {
        let vtl = vtl_transform(vtl);
        let value = AlignedU128::from(value);
        let reg_value = hvdef::HvRegisterValue(value);
        self.hvcall
            .set_register(hvdef::HvRegisterName(register_index), reg_value, Some(vtl))
            .map_err(|e| e.into())
    }

    fn get_vp_state_with_vtl(&mut self, register_index: u32, vtl: Vtl) -> TmkResult<u64> {
        let vtl = vtl_transform(vtl);
        self.hvcall
            .get_register(hvdef::HvRegisterName(register_index), Some(vtl))
            .map(|v| v.as_u64())
            .map_err(|e| e.into())
    }
}

fn vtl_transform(vtl: Vtl) -> HvInputVtl {
    let vtl = match vtl {
        Vtl::Vtl0 => 0,
        Vtl::Vtl1 => 1,
        Vtl::Vtl2 => 2,
    };
    HvInputVtl::new()
        .with_target_vtl_value(vtl)
        .with_use_target_vtl(true)
}

#[cfg_attr(target_arch = "aarch64", expect(dead_code))]
impl HvTestCtx {
    /// Construct an *un-initialised* test context.  
    /// Call [`HvTestCtx::init`] before using the value.
    pub const fn new() -> Self {
        HvTestCtx {
            hvcall: HvCall::new(),
            vp_running: BTreeSet::new(),
            my_vp_idx: 0,
            my_vtl: Vtl::Vtl0,
        }
    }

    /// Perform the one-time initialisation sequence:  
    /// – initialise the hypercall page,  
    /// – discover the VP count and create command queues,  
    /// – record the current VTL.
    pub fn init(&mut self, vtl: Vtl) -> TmkResult<()> {
        self.hvcall.initialize();
        let vp_count = self.get_vp_count()?;
        for i in 0..vp_count {
            register_command_queue(i);
        }
        self.my_vtl = vtl;
        // let reg = self
        //     .hvcall
        //     .get_register(hvdef::HvAllArchRegisterName::VpIndex.into(), None)
        //     .expect("error: failed to get vp index");
        // let reg = reg.as_u64();
        // self.my_vp_idx = reg as u32;

        self.my_vp_idx = Self::get_vp_idx();
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    fn get_vp_idx() -> u32 {
        let result = unsafe { core::arch::x86_64::__cpuid(0x1) };
        (result.ebx >> 24) & 0xFF
    }

    #[cfg(target_arch = "aarch64")]
    fn get_vp_idx() -> u32 {
        unimplemented!()
    }

    fn secure_exec_handler() {
        HvTestCtx::exec_handler(Vtl::Vtl1);
    }

    fn general_exec_handler() {
        HvTestCtx::exec_handler(Vtl::Vtl0);
    }

    /// Busy-loop executor that runs on every VP.  
    /// Extracts commands from the per-VP queue and executes them in the
    /// appropriate VTL, switching VTLs when necessary.
    fn exec_handler(vtl: Vtl) {
        let mut ctx = HvTestCtx::new();
        ctx.init(vtl).expect("error: failed to init on a VP");
        loop {
            let mut vtl: Option<Vtl> = None;
            let mut cmd: Option<Box<dyn FnOnce(&mut HvTestCtx) + 'static>> = None;

            {
                let mut cmdt = cmdt().lock();
                let d = cmdt.get_mut(&ctx.my_vp_idx);
                if let Some(d) = d {
                    if !d.is_empty() {
                        let (_c, v) = d.front().unwrap();
                        if *v == ctx.my_vtl {
                            let (c, _v) = d.pop_front().unwrap();
                            cmd = Some(c);
                        } else {
                            vtl = Some(*v);
                        }
                    }
                }
            }

            if let Some(vtl) = vtl {
                if vtl == Vtl::Vtl0 {
                    ctx.switch_to_low_vtl();
                } else {
                    ctx.switch_to_high_vtl();
                }
            }

            if let Some(cmd) = cmd {
                cmd(&mut ctx);
            }
        }
    }

    #[cfg(target_arch = "x86_64")]
    /// Capture the current VP context, patch the entry point and stack
    /// so that the new VP starts in `exec_handler`.
    fn get_default_context(&mut self, vtl: Vtl) -> Result<InitialVpContextX64, TmkError> {
        let handler = match vtl {
            Vtl::Vtl0 => HvTestCtx::general_exec_handler,
            Vtl::Vtl1 => HvTestCtx::secure_exec_handler,
            _ => return Err(TmkError::InvalidParameter.into()),
        };
        self.run_fn_with_current_context(handler)
    }

    #[cfg(target_arch = "aarch64")]
    /// Capture the current VP context, patch the entry point and stack
    /// so that the new VP starts in `exec_handler`.
    fn get_default_context(&mut self, _vtl: Vtl) -> Result<InitialVpContextArm64, TmkError> {
        use core::panic;
        panic!("aarch64 not implemented");
    }

    #[cfg(target_arch = "x86_64")]
    /// Helper to wrap an arbitrary function inside a captured VP context
    /// that can later be used to start a new VP/VTL instance.
    fn run_fn_with_current_context(&mut self, func: fn()) -> Result<InitialVpContextX64, TmkError> {
        let mut vp_context: InitialVpContextX64 = self
            .hvcall
            .get_current_vtl_vp_context()
            .expect("Failed to get VTL1 context");
        let stack_layout = Layout::from_size_align(1024 * 1024, 16)
            .expect("Failed to create layout for stack allocation");
        let allocated_stack_ptr = unsafe { alloc(stack_layout) };
        if allocated_stack_ptr.is_null() {
            return Err(TmkError::AllocationFailed.into());
        }
        let stack_size = stack_layout.size();
        let stack_top = allocated_stack_ptr as u64 + stack_size as u64;
        let fn_ptr = func as fn();
        let fn_address = fn_ptr as u64;
        vp_context.rip = fn_address;
        vp_context.rsp = stack_top;
        Ok(vp_context)
    }

    // function to print the current register states for x64
    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    pub fn print_rbp(&self) {
        let rbp: u64;
        unsafe {
            asm!(
                "mov {}, rbp",
                out(reg) rbp,
            );
        }
        log::debug!(
            "Current RBP: 0x{:#x}, VP:{} VTL:{:?}",
            rbp,
            self.my_vp_idx,
            self.my_vtl
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    pub fn print_rsp(&self) {
        let rsp: u64;
        unsafe {
            asm!(
                "mov {}, rsp",
                out(reg) rsp,
            );
        }
        log::debug!(
            "Current RSP: 0x{:#x}, VP:{} VTL:{:?}",
            rsp,
            self.my_vp_idx,
            self.my_vtl
        );
    }
}

impl From<hvdef::HvError> for TmkError {
    fn from(e: hvdef::HvError) -> Self {
        log::debug!("Converting hvdef::HvError::{:?} to TmkError", e);
        let tmk_error_type = match e {
            hvdef::HvError::InvalidHypercallCode => TmkError::InvalidHypercallCode,
            hvdef::HvError::InvalidHypercallInput => TmkError::InvalidHypercallInput,
            hvdef::HvError::InvalidAlignment => TmkError::InvalidAlignment,
            hvdef::HvError::InvalidParameter => TmkError::InvalidParameter,
            hvdef::HvError::AccessDenied => TmkError::AccessDenied,
            hvdef::HvError::InvalidPartitionState => TmkError::InvalidPartitionState,
            hvdef::HvError::OperationDenied => TmkError::OperationDenied,
            hvdef::HvError::UnknownProperty => TmkError::UnknownProperty,
            hvdef::HvError::PropertyValueOutOfRange => TmkError::PropertyValueOutOfRange,
            hvdef::HvError::InsufficientMemory => TmkError::InsufficientMemory,
            hvdef::HvError::PartitionTooDeep => TmkError::PartitionTooDeep,
            hvdef::HvError::InvalidPartitionId => TmkError::InvalidPartitionId,
            hvdef::HvError::InvalidVpIndex => TmkError::InvalidVpIndex,
            hvdef::HvError::NotFound => TmkError::NotFound,
            hvdef::HvError::InvalidPortId => TmkError::InvalidPortId,
            hvdef::HvError::InvalidConnectionId => TmkError::InvalidConnectionId,
            hvdef::HvError::InsufficientBuffers => TmkError::InsufficientBuffers,
            hvdef::HvError::NotAcknowledged => TmkError::NotAcknowledged,
            hvdef::HvError::InvalidVpState => TmkError::InvalidVpState,
            hvdef::HvError::Acknowledged => TmkError::Acknowledged,
            hvdef::HvError::InvalidSaveRestoreState => TmkError::InvalidSaveRestoreState,
            hvdef::HvError::InvalidSynicState => TmkError::InvalidSynicState,
            hvdef::HvError::ObjectInUse => TmkError::ObjectInUse,
            hvdef::HvError::InvalidProximityDomainInfo => TmkError::InvalidProximityDomainInfo,
            hvdef::HvError::NoData => TmkError::NoData,
            hvdef::HvError::Inactive => TmkError::Inactive,
            hvdef::HvError::NoResources => TmkError::NoResources,
            hvdef::HvError::FeatureUnavailable => TmkError::FeatureUnavailable,
            hvdef::HvError::PartialPacket => TmkError::PartialPacket,
            hvdef::HvError::ProcessorFeatureNotSupported => {
                TmkError::ProcessorFeatureNotSupported
            }
            hvdef::HvError::ProcessorCacheLineFlushSizeIncompatible => {
                TmkError::ProcessorCacheLineFlushSizeIncompatible
            }
            hvdef::HvError::InsufficientBuffer => TmkError::InsufficientBuffer,
            hvdef::HvError::IncompatibleProcessor => TmkError::IncompatibleProcessor,
            hvdef::HvError::InsufficientDeviceDomains => TmkError::InsufficientDeviceDomains,
            hvdef::HvError::CpuidFeatureValidationError => {
                TmkError::CpuidFeatureValidationError
            }
            hvdef::HvError::CpuidXsaveFeatureValidationError => {
                TmkError::CpuidXsaveFeatureValidationError
            }
            hvdef::HvError::ProcessorStartupTimeout => TmkError::ProcessorStartupTimeout,
            hvdef::HvError::SmxEnabled => TmkError::SmxEnabled,
            hvdef::HvError::InvalidLpIndex => TmkError::InvalidLpIndex,
            hvdef::HvError::InvalidRegisterValue => TmkError::InvalidRegisterValue,
            hvdef::HvError::InvalidVtlState => TmkError::InvalidVtlState,
            hvdef::HvError::NxNotDetected => TmkError::NxNotDetected,
            hvdef::HvError::InvalidDeviceId => TmkError::InvalidDeviceId,
            hvdef::HvError::InvalidDeviceState => TmkError::InvalidDeviceState,
            hvdef::HvError::PendingPageRequests => TmkError::PendingPageRequests,
            hvdef::HvError::PageRequestInvalid => TmkError::PageRequestInvalid,
            hvdef::HvError::KeyAlreadyExists => TmkError::KeyAlreadyExists,
            hvdef::HvError::DeviceAlreadyInDomain => TmkError::DeviceAlreadyInDomain,
            hvdef::HvError::InvalidCpuGroupId => TmkError::InvalidCpuGroupId,
            hvdef::HvError::InvalidCpuGroupState => TmkError::InvalidCpuGroupState,
            hvdef::HvError::OperationFailed => TmkError::OperationFailed,
            hvdef::HvError::NotAllowedWithNestedVirtActive => {
                TmkError::NotAllowedWithNestedVirtActive
            }
            hvdef::HvError::InsufficientRootMemory => TmkError::InsufficientRootMemory,
            hvdef::HvError::EventBufferAlreadyFreed => TmkError::EventBufferAlreadyFreed,
            hvdef::HvError::Timeout => TmkError::Timeout,
            hvdef::HvError::VtlAlreadyEnabled => TmkError::VtlAlreadyEnabled,
            hvdef::HvError::UnknownRegisterName => TmkError::UnknownRegisterName,
            // Add any other specific mappings here if hvdef::HvError has more variants
            _ => {
                log::warn!(
                    "Unhandled hvdef::HvError variant: {:?}. Mapping to TmkError::OperationFailed.",
                    e
                );
                TmkError::OperationFailed // Generic fallback
            }
        };
        log::debug!(
            "Mapped hvdef::HvError::{:?} to TmkError::{:?}",
            e,
            tmk_error_type
        );
        tmk_error_type
    }
}
