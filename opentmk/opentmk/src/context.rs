#![allow(dead_code)]
use core::ops::Range;

use alloc::boxed::Box;
use hvdef::Vtl;

use crate::tmkdefs::TmkResult;


pub trait SecureInterceptPlatformTrait {
    fn setup_secure_intercept(&mut self, interrupt_idx: u8) -> TmkResult<()>;
}

pub trait InterruptPlatformTrait {
    fn set_interrupt_idx(&mut self, interrupt_idx: u8, handler: fn()) -> TmkResult<()>;
    fn setup_interrupt_handler(&mut self) -> TmkResult<()>;
}

pub trait MsrPlatformTrait {
    fn read_msr(&mut self, msr: u32) -> TmkResult<u64>;
    fn write_msr(&mut self, msr: u32, value: u64) -> TmkResult<()>;
}

pub trait VirtualProcessorPlatformTrait<T> where T: VtlPlatformTrait {
    fn get_current_vp(&self) -> TmkResult<u32>;
    fn get_register(&mut self, reg: u32) -> TmkResult<u128>;
    fn get_vp_count(&self) -> TmkResult<u32>;
    fn queue_command_vp(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;
    fn start_on_vp(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;
    fn start_running_vp_with_default_context(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;
}

pub trait VtlPlatformTrait {
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl) -> TmkResult<()>;
    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()>; 
    fn get_current_vtl(&self) -> TmkResult<Vtl>;
    fn set_default_ctx_to_vp(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()>;
    fn setup_partition_vtl(&mut self, vtl: Vtl) -> TmkResult<()>;
    fn setup_vtl_protection(&mut self) -> TmkResult<()>;
    fn switch_to_high_vtl(&mut self);
    fn switch_to_low_vtl(&mut self);
}

pub trait X64PlatformTrait {}
pub trait Aarch64PlatformTrait {}

pub struct VpExecutor<T> {
    vp_index: u32,
    vtl: Vtl,
    cmd: Option<Box<dyn FnOnce(&mut T)>>,
}

impl<T> VpExecutor<T> {
    pub fn new(vp_index: u32, vtl: Vtl) -> Self {
        VpExecutor {
            vp_index,
            vtl,
            cmd: None,
        }
    }

    pub fn command(mut self, cmd: impl FnOnce(&mut T) + 'static) -> Self {
        self.cmd = Some(Box::new(cmd));
        self
    }

    pub fn get(mut self) -> (u32, Vtl, Option<Box<dyn FnOnce(&mut T)>>) {
        let cmd = self.cmd.take();
        (self.vp_index, self.vtl, cmd)
    }
}