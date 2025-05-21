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

pub trait TestCtxTrait<T> {
    // partition wide Traits
    /// Returns the number of virtual processors (VPs) in the partition.
    fn get_vp_count(&self) -> u32;
    /// Sets up VTL (Virtualization Trust Level) protection for the partition.
    fn setup_vtl_protection(&mut self) -> TmkResult<()>;
    /// Sets up a specific VTL for the partition.
    fn setup_partition_vtl(&mut self, vtl: Vtl) -> TmkResult<()>;
    /// Sets up the interrupt handler for the partition.
    fn setup_interrupt_handler(&mut self) -> TmkResult<()>;
    /// Sets the interrupt handler for a specific interrupt index.
    fn set_interrupt_idx(&mut self, interrupt_idx: u8, handler: fn()) -> TmkResult<()>;
    /// Starts a command on a specific virtual processor.
    fn start_on_vp(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;
    /// Queues a command to be executed on a virtual processor.
    fn queue_command_vp(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;
    /// Sets up a secure intercept for a given interrupt index.
    fn setup_secure_intercept(&mut self, interrupt_idx: u8) -> TmkResult<()>;
    /// Applies VTL protection to a specified memory range.
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl) -> TmkResult<()>;
    /// Sets the default context for a specific virtual processor and VTL.
    fn set_default_ctx_to_vp(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()>;
    /// Starts running a virtual processor with its default context.
    fn start_running_vp_with_default_context(&mut self, cmd: VpExecutor<T>)-> TmkResult<()>;
    /// Enables VTL for a virtual processor using its default context.
    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl)-> TmkResult<()>;
    /// Writes a value to a specific Model Specific Register (MSR).
    fn write_msr(&mut self, msr: u32, value: u64)-> TmkResult<()>;
    /// Reads a value from a specific Model Specific Register (MSR).
    fn read_msr(&mut self, msr: u32) -> TmkResult<u64>;

    // per vp wide Traits
    /// Gets the index of the current virtual processor.
    fn get_current_vp(&self) -> TmkResult<u32>;
    /// Gets the current VTL of the virtual processor.
    fn get_current_vtl(&self) -> TmkResult<Vtl>;
    /// Switches the current virtual processor to a higher VTL.
    fn switch_to_high_vtl(&mut self);
    /// Switches the current virtual processor to a lower VTL.
    fn switch_to_low_vtl(&mut self);
    /// Gets the value of a specific register for the current virtual processor.
    fn get_register(&mut self, reg: u32) -> TmkResult<u128>;

}

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