use crate::uefi::alloc::ALLOCATOR;
use crate::{
    context::{
        InterruptPlatformTrait, MsrPlatformTrait, SecureInterceptPlatformTrait,
        VirtualProcessorPlatformTrait, VpExecutor, VtlPlatformTrait,
    },
    hypercall::HvCall,
    tmkdefs::{TmkError, TmkErrorType, TmkResult},
};

use alloc::boxed::Box;
use alloc::collections::linked_list::LinkedList;
use alloc::collections::{btree_map::BTreeMap, btree_set::BTreeSet};
use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::ops::Range;
use hvdef::hypercall::{HvInputVtl, InitialVpContextX64};
use hvdef::Vtl;
use memory_range::MemoryRange;
use minimal_rt::arch::msr::{read_msr, write_msr};
use sync_nostd::Mutex;

const ALIGNMENT: usize = 4096;

type ComandTable = BTreeMap<u32, LinkedList<(Box<dyn FnOnce(&mut HvTestCtx) + 'static>, Vtl)>>;
static mut CMD: Mutex<ComandTable> = Mutex::new(BTreeMap::new());

#[allow(static_mut_refs)]
fn cmdt() -> &'static Mutex<ComandTable> {
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
    pub vp_runing: BTreeSet<u32>,
    pub my_vp_idx: u32,
    pub my_vtl: Vtl,
}

impl Drop for HvTestCtx {
    fn drop(&mut self) {
        self.hvcall.uninitialize();
    }
}


impl SecureInterceptPlatformTrait for HvTestCtx {
    fn setup_secure_intercept(&mut self, interrupt_idx: u8) -> TmkResult<()> {
        let layout = Layout::from_size_align(4096, ALIGNMENT)
            .or_else(|_| Err(TmkError(TmkErrorType::AllocationFailed)))?;

        let ptr = unsafe { ALLOCATOR.alloc(layout) };
        let gpn = (ptr as u64) >> 12;
        let reg = (gpn << 12) | 0x1;

        unsafe { write_msr(hvdef::HV_X64_MSR_SIMP, reg.into()) };
        log::info!("Successfuly set the SIMP register.");

        let reg = unsafe { read_msr(hvdef::HV_X64_MSR_SINT0) };
        let mut reg: hvdef::HvSynicSint = reg.into();
        reg.set_vector(interrupt_idx);
        reg.set_masked(false);
        reg.set_auto_eoi(true);

        self.write_msr(hvdef::HV_X64_MSR_SINT0, reg.into())?;
        log::info!("Successfuly set the SINT0 register.");
        Ok(())
    }
}



impl InterruptPlatformTrait for HvTestCtx {
    fn set_interrupt_idx(&mut self, interrupt_idx: u8, handler: fn()) -> TmkResult<()> {
        #[cfg(target_arch = "x86_64")]
        {
            crate::arch::interrupt::set_handler(interrupt_idx, handler);
            Ok(())
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(TmkError(TmkErrorType::NotImplemented))
        }
    }

    fn setup_interrupt_handler(&mut self) -> TmkResult<()> {
        crate::arch::interrupt::init();
        Ok(())
    }
}



impl MsrPlatformTrait for HvTestCtx {
    fn read_msr(&mut self, msr: u32) -> TmkResult<u64> {
        let r = unsafe { read_msr(msr) };
        Ok(r)
    }

    fn write_msr(&mut self, msr: u32, value: u64) -> TmkResult<()> {
        unsafe { write_msr(msr, value) };
        Ok(())
    }
}



impl VirtualProcessorPlatformTrait<HvTestCtx> for HvTestCtx {
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
            use hvdef::HvAarch64RegisterName;
            let reg = HvAarch64RegisterName(reg);
            let val = self.hvcall.get_register(reg.into(), None)?.as_u128();
            Ok(val)
        }

        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Err(TmkError(TmkErrorType::NotImplemented))
        }
    }

    fn get_vp_count(&self) -> TmkResult<u32> {
        #[cfg(target_arch = "x86_64")]
        {
            let mut result: u32;
            unsafe {
                asm!(
                    "push rbx",
                    "cpuid",
                    "mov {result:r}, rbx",
                    "pop rbx",
                    in("eax") 1u32,
                    out("ecx") _,
                    out("edx") _,
                    result = out(reg) result,
                    options(nomem, nostack)
                );
            }
            Ok((result >> 16) & 0xFF)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(TmkError(TmkErrorType::NotImplemented))
        }
    }

    fn queue_command_vp(&mut self, cmd: VpExecutor<HvTestCtx>) -> TmkResult<()> {
        let (vp_index, vtl, cmd) = cmd.get();
        let cmd = cmd.ok_or_else(|| TmkError(TmkErrorType::QueueCommandFailed))?;
        cmdt()
            .lock()
            .get_mut(&vp_index)
            .unwrap()
            .push_back((cmd, vtl));
        Ok(())
    }

    fn start_on_vp(&mut self, cmd: VpExecutor<HvTestCtx>) -> TmkResult<()> {
        let (vp_index, vtl, cmd) = cmd.get();
        let cmd = cmd.ok_or_else(|| TmkError(TmkErrorType::InvalidParameter))?;
        if vtl >= Vtl::Vtl2 {
            return Err(TmkError(TmkErrorType::InvalidParameter));
        }

        let is_vp_running = self.vp_runing.get(&vp_index);
        if let Some(_running_vtl) = is_vp_running {
            log::debug!("both vtl0 and vtl1 are running for VP: {:?}", vp_index);
        } else {
            if vp_index == 0 {
                let vp_context = self.get_default_context()?;
                self.hvcall.enable_vp_vtl(0, Vtl::Vtl1, Some(vp_context))?;

                cmdt().lock().get_mut(&vp_index).unwrap().push_back((
                    Box::new(move |ctx| {
                        ctx.switch_to_low_vtl();
                    }),
                    Vtl::Vtl1,
                ));
                self.switch_to_high_vtl();
                self.vp_runing.insert(vp_index);
            } else {
                let (tx, rx) = sync_nostd::Channel::<TmkResult<()>>::new().split();
                cmdt().lock().get_mut(&self.my_vp_idx).unwrap().push_back((
                    Box::new(move |ctx| {
                        let r = ctx.enable_vp_vtl_with_default_context(vp_index, Vtl::Vtl1);
                        if r.is_err() {
                            let _ = tx.send(r);
                            return;
                        }
                        let r = ctx.start_running_vp_with_default_context(VpExecutor::new(
                            vp_index,
                            Vtl::Vtl0,
                        ));
                        if r.is_err() {
                            let _ = tx.send(r);
                            return;
                        }
                        let _ = tx.send(Ok(()));
                        ctx.switch_to_low_vtl();
                    }),
                    Vtl::Vtl1,
                ));
                self.switch_to_high_vtl();
                log::debug!("VP{} waiting for start confirmation for vp from VTL1: {}", self.my_vp_idx, vp_index);
                let rx = rx.recv();
                if let Ok(r) = rx {
                    r?;
                }
                self.vp_runing.insert(vp_index);
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

    fn start_running_vp_with_default_context(
        &mut self,
        cmd: VpExecutor<HvTestCtx>,
    ) -> TmkResult<()> {
        let (vp_index, vtl, _cmd) = cmd.get();
        let vp_ctx = self.get_default_context()?;
        self.hvcall
            .start_virtual_processor(vp_index, vtl, Some(vp_ctx))?;
        Ok(())
    }

    fn get_current_vp(&self) -> TmkResult<u32> {
        Ok(self.my_vp_idx)
    }
}

impl VtlPlatformTrait for HvTestCtx {
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl) -> TmkResult<()> {
        self.hvcall
            .apply_vtl_protections(MemoryRange::new(range), vtl)?;
        Ok(())
    }

    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()> {
        let vp_ctx = self.get_default_context()?;
        self.hvcall.enable_vp_vtl(vp_index, vtl, Some(vp_ctx))?;
        Ok(())
    }

    fn get_current_vtl(&self) -> TmkResult<Vtl> {
        Ok(self.my_vtl)
    }

    fn set_default_ctx_to_vp(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()> {
        let i: u8 = match vtl {
            Vtl::Vtl0 => 0,
            Vtl::Vtl1 => 1,
            Vtl::Vtl2 => 2,
        };
        let vp_context = self.get_default_context()?;
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

    fn setup_partition_vtl(&mut self, vtl: Vtl) -> TmkResult<()> {
        self.hvcall
            .enable_partition_vtl(hvdef::HV_PARTITION_ID_SELF, vtl)?;
        log::info!("enabled vtl protections for the partition.");
        Ok(())
    }

    fn setup_vtl_protection(&mut self) -> TmkResult<()> {
        self.hvcall.enable_vtl_protection(HvInputVtl::CURRENT_VTL)?;
        log::info!("enabled vtl protections for the partition.");
        Ok(())
    }

    fn switch_to_high_vtl(&mut self) {
        HvCall::vtl_call();
    }

    fn switch_to_low_vtl(&mut self) {
        HvCall::vtl_return();
    }
}
impl HvTestCtx {
    pub const fn new() -> Self {
        HvTestCtx {
            hvcall: HvCall::new(),
            vp_runing: BTreeSet::new(),
            my_vp_idx: 0,
            my_vtl: Vtl::Vtl0,
        }
    }

    pub fn init(&mut self) -> TmkResult<()> {
        self.hvcall.initialize();
        let vp_count = self.get_vp_count()?;
        for i in 0..vp_count {
            register_command_queue(i);
        }
        self.my_vtl = self.hvcall.vtl();
        Ok(())
    }

    fn exec_handler() {
        let mut ctx = HvTestCtx::new();
        ctx.init().expect("error: failed to init on a VP");
        let reg = ctx
            .hvcall
            .get_register(hvdef::HvAllArchRegisterName::VpIndex.into(), None)
            .expect("error: failed to get vp index");
        let reg = reg.as_u64();
        ctx.my_vp_idx = reg as u32;

        loop {
            let mut vtl: Option<Vtl> = None;
            let mut cmd: Option<Box<dyn FnOnce(&mut HvTestCtx) + 'static>> = None;

            {
                let mut cmdt = cmdt().lock();
                let d = cmdt.get_mut(&ctx.my_vp_idx);
                if d.is_some() {
                    let d = d.unwrap();
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
    fn get_default_context(&mut self) -> Result<InitialVpContextX64, TmkError> {
        return self.run_fn_with_current_context(HvTestCtx::exec_handler);
    }

    #[cfg(target_arch = "x86_64")]
    fn run_fn_with_current_context(&mut self, func: fn()) -> Result<InitialVpContextX64, TmkError> {
        use super::alloc::SIZE_1MB;

        let mut vp_context: InitialVpContextX64 = self
            .hvcall
            .get_current_vtl_vp_context()
            .expect("Failed to get VTL1 context");
        let stack_layout = Layout::from_size_align(SIZE_1MB, 16)
            .expect("Failed to create layout for stack allocation");
        let allocated_stack_ptr = unsafe { ALLOCATOR.alloc(stack_layout) };
        if allocated_stack_ptr.is_null() {
            return Err(TmkErrorType::AllocationFailed.into());
        }
        let stack_size = stack_layout.size();
        let stack_top = allocated_stack_ptr as u64 + stack_size as u64;
        let fn_ptr = func as fn();
        let fn_address = fn_ptr as u64;
        vp_context.rip = fn_address;
        vp_context.rsp = stack_top;
        Ok(vp_context)
    }
}

impl From<hvdef::HvError> for TmkError {
    fn from(e: hvdef::HvError) -> Self {
        log::debug!("Converting hvdef::HvError::{:?} to TmkError", e);
        let tmk_error_type = match e {
            hvdef::HvError::InvalidHypercallCode => TmkErrorType::InvalidHypercallCode,
            hvdef::HvError::InvalidHypercallInput => TmkErrorType::InvalidHypercallInput,
            hvdef::HvError::InvalidAlignment => TmkErrorType::InvalidAlignment,
            hvdef::HvError::InvalidParameter => TmkErrorType::InvalidParameter,
            hvdef::HvError::AccessDenied => TmkErrorType::AccessDenied,
            hvdef::HvError::InvalidPartitionState => TmkErrorType::InvalidPartitionState,
            hvdef::HvError::OperationDenied => TmkErrorType::OperationDenied,
            hvdef::HvError::UnknownProperty => TmkErrorType::UnknownProperty,
            hvdef::HvError::PropertyValueOutOfRange => TmkErrorType::PropertyValueOutOfRange,
            hvdef::HvError::InsufficientMemory => TmkErrorType::InsufficientMemory,
            hvdef::HvError::PartitionTooDeep => TmkErrorType::PartitionTooDeep,
            hvdef::HvError::InvalidPartitionId => TmkErrorType::InvalidPartitionId,
            hvdef::HvError::InvalidVpIndex => TmkErrorType::InvalidVpIndex,
            hvdef::HvError::NotFound => TmkErrorType::NotFound,
            hvdef::HvError::InvalidPortId => TmkErrorType::InvalidPortId,
            hvdef::HvError::InvalidConnectionId => TmkErrorType::InvalidConnectionId,
            hvdef::HvError::InsufficientBuffers => TmkErrorType::InsufficientBuffers,
            hvdef::HvError::NotAcknowledged => TmkErrorType::NotAcknowledged,
            hvdef::HvError::InvalidVpState => TmkErrorType::InvalidVpState,
            hvdef::HvError::Acknowledged => TmkErrorType::Acknowledged,
            hvdef::HvError::InvalidSaveRestoreState => TmkErrorType::InvalidSaveRestoreState,
            hvdef::HvError::InvalidSynicState => TmkErrorType::InvalidSynicState,
            hvdef::HvError::ObjectInUse => TmkErrorType::ObjectInUse,
            hvdef::HvError::InvalidProximityDomainInfo => TmkErrorType::InvalidProximityDomainInfo,
            hvdef::HvError::NoData => TmkErrorType::NoData,
            hvdef::HvError::Inactive => TmkErrorType::Inactive,
            hvdef::HvError::NoResources => TmkErrorType::NoResources,
            hvdef::HvError::FeatureUnavailable => TmkErrorType::FeatureUnavailable,
            hvdef::HvError::PartialPacket => TmkErrorType::PartialPacket,
            hvdef::HvError::ProcessorFeatureNotSupported => {
                TmkErrorType::ProcessorFeatureNotSupported
            }
            hvdef::HvError::ProcessorCacheLineFlushSizeIncompatible => {
                TmkErrorType::ProcessorCacheLineFlushSizeIncompatible
            }
            hvdef::HvError::InsufficientBuffer => TmkErrorType::InsufficientBuffer,
            hvdef::HvError::IncompatibleProcessor => TmkErrorType::IncompatibleProcessor,
            hvdef::HvError::InsufficientDeviceDomains => TmkErrorType::InsufficientDeviceDomains,
            hvdef::HvError::CpuidFeatureValidationError => {
                TmkErrorType::CpuidFeatureValidationError
            }
            hvdef::HvError::CpuidXsaveFeatureValidationError => {
                TmkErrorType::CpuidXsaveFeatureValidationError
            }
            hvdef::HvError::ProcessorStartupTimeout => TmkErrorType::ProcessorStartupTimeout,
            hvdef::HvError::SmxEnabled => TmkErrorType::SmxEnabled,
            hvdef::HvError::InvalidLpIndex => TmkErrorType::InvalidLpIndex,
            hvdef::HvError::InvalidRegisterValue => TmkErrorType::InvalidRegisterValue,
            hvdef::HvError::InvalidVtlState => TmkErrorType::InvalidVtlState,
            hvdef::HvError::NxNotDetected => TmkErrorType::NxNotDetected,
            hvdef::HvError::InvalidDeviceId => TmkErrorType::InvalidDeviceId,
            hvdef::HvError::InvalidDeviceState => TmkErrorType::InvalidDeviceState,
            hvdef::HvError::PendingPageRequests => TmkErrorType::PendingPageRequests,
            hvdef::HvError::PageRequestInvalid => TmkErrorType::PageRequestInvalid,
            hvdef::HvError::KeyAlreadyExists => TmkErrorType::KeyAlreadyExists,
            hvdef::HvError::DeviceAlreadyInDomain => TmkErrorType::DeviceAlreadyInDomain,
            hvdef::HvError::InvalidCpuGroupId => TmkErrorType::InvalidCpuGroupId,
            hvdef::HvError::InvalidCpuGroupState => TmkErrorType::InvalidCpuGroupState,
            hvdef::HvError::OperationFailed => TmkErrorType::OperationFailed,
            hvdef::HvError::NotAllowedWithNestedVirtActive => {
                TmkErrorType::NotAllowedWithNestedVirtActive
            }
            hvdef::HvError::InsufficientRootMemory => TmkErrorType::InsufficientRootMemory,
            hvdef::HvError::EventBufferAlreadyFreed => TmkErrorType::EventBufferAlreadyFreed,
            hvdef::HvError::Timeout => TmkErrorType::Timeout,
            hvdef::HvError::VtlAlreadyEnabled => TmkErrorType::VtlAlreadyEnabled,
            hvdef::HvError::UnknownRegisterName => TmkErrorType::UnknownRegisterName,
            // Add any other specific mappings here if hvdef::HvError has more variants
            _ => {
                log::warn!(
                    "Unhandled hvdef::HvError variant: {:?}. Mapping to TmkErrorType::OperationFailed.",
                    e
                );
                TmkErrorType::OperationFailed // Generic fallback
            }
        };
        log::debug!(
            "Mapped hvdef::HvError::{:?} to TmkErrorType::{:?}",
            e,
            tmk_error_type
        );
        TmkError(tmk_error_type)
    }
}
