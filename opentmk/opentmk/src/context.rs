#![allow(dead_code)]
use core::ops::Range;

use alloc::boxed::Box;
use hvdef::Vtl;

use crate::tmkdefs::TmkResult;

pub trait SecureInterceptPlatformTrait {
    /// Installs a secure-world intercept for the given interrupt.
    ///
    /// The platform must arrange that the supplied `interrupt_idx`
    /// triggers a VM-exit or any other mechanism that transfers control
    /// to the TMK secure handler.
    ///
    /// Returns `Ok(())` on success or an error wrapped in `TmkResult`.
    fn setup_secure_intercept(&mut self, interrupt_idx: u8) -> TmkResult<()>;
}

pub trait InterruptPlatformTrait {
    /// Associates an interrupt vector with a handler inside the
    /// non-secure world.
    ///
    /// * `interrupt_idx` – IDT/GIC index to program  
    /// * `handler` – Function that will be executed when the interrupt
    ///   fires.
    fn set_interrupt_idx(&mut self, interrupt_idx: u8, handler: fn()) -> TmkResult<()>;

    /// Finalises platform specific interrupt setup (enables the table,
    /// unmasks lines, etc.).
    fn setup_interrupt_handler(&mut self) -> TmkResult<()>;
}

pub trait MsrPlatformTrait {
    /// Reads the content of `msr`.
    ///
    /// Returns the 64-bit value currently stored in that MSR.
    fn read_msr(&mut self, msr: u32) -> TmkResult<u64>;

    /// Writes `value` into `msr`.
    fn write_msr(&mut self, msr: u32, value: u64) -> TmkResult<()>;
}

pub trait VirtualProcessorPlatformTrait<T>
where
    T: VtlPlatformTrait,
{
    /// Returns the index of the virtual CPU currently executing this
    /// code.
    fn get_current_vp(&self) -> TmkResult<u32>;

    /// Reads the architecture specific register identified by `reg`.
    fn get_register(&mut self, reg: u32) -> TmkResult<u128>;

    /// Total number of online VPs in the partition.
    fn get_vp_count(&self) -> TmkResult<u32>;

    /// Queues `cmd` to run later on the VP described inside the
    /// `VpExecutor`.
    fn queue_command_vp(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;

    /// Synchronously executes `cmd` on its target VP.
    fn start_on_vp(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;

    /// Starts the target VP (if required) and executes `cmd` with a
    /// platform provided default VTL context.
    fn start_running_vp_with_default_context(&mut self, cmd: VpExecutor<T>) -> TmkResult<()>;
}

pub trait VtlPlatformTrait {
    /// Applies VTL protection to the supplied physical address range.
    fn apply_vtl_protection_for_memory(&mut self, range: Range<u64>, vtl: Vtl) -> TmkResult<()>;

    /// Enables the given `vtl` on `vp_index` with a default context.
    fn enable_vp_vtl_with_default_context(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()>;

    /// Returns the VTL level the caller is currently executing in.
    fn get_current_vtl(&self) -> TmkResult<Vtl>;

    /// Sets the default VTL context on `vp_index`.
    fn set_default_ctx_to_vp(&mut self, vp_index: u32, vtl: Vtl) -> TmkResult<()>;

    /// Performs partition wide initialisation for a given `vtl`.
    fn setup_partition_vtl(&mut self, vtl: Vtl) -> TmkResult<()>;

    /// Platform specific global VTL preparation (stage 2 translation,
    /// EPT, etc.).
    fn setup_vtl_protection(&mut self) -> TmkResult<()>;

    /// Switches the current hardware thread to the higher privileged VTL.
    fn switch_to_high_vtl(&mut self);

    /// Switches the current hardware thread back to the lower privileged VTL.
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
    /// Creates a new executor targeting `vp_index` running in `vtl`.
    pub fn new(vp_index: u32, vtl: Vtl) -> Self {
        VpExecutor {
            vp_index,
            vtl,
            cmd: None,
        }
    }

    /// Stores a closure `cmd` that will be executed on the target VP.
    ///
    /// The closure receives a mutable reference to the platform-specific
    /// type `T` that implements `VtlPlatformTrait`.
    pub fn command(mut self, cmd: impl FnOnce(&mut T) + 'static) -> Self {
        self.cmd = Some(Box::new(cmd));
        self
    }

    /// Extracts the tuple `(vp_index, vtl, cmd)` consuming `self`.
    pub fn get(mut self) -> (u32, Vtl, Option<Box<dyn FnOnce(&mut T)>>) {
        let cmd = self.cmd.take();
        (self.vp_index, self.vtl, cmd)
    }
}