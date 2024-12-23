// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::VmProxy;
use gdbstub::target::ext::target_description_xml_override::TargetDescriptionXmlOverrideOps;
use gdbstub::target::Target;
use gdbstub::target::TargetError;
use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;
use vmm_core_defs::debug_rpc::DebuggerVpState;

mod base;
mod breakpoints;
mod target_aarch64;
mod target_i8086;
mod target_x86_64_qemu;

pub trait ToTargetResult<T, E> {
    fn fatal(self) -> Result<T, TargetError<E>>;
    fn nonfatal(self) -> Result<T, TargetError<E>>;
}

impl<T, E> ToTargetResult<T, anyhow::Error> for Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn fatal(self) -> Result<T, TargetError<anyhow::Error>> {
        self.map_err(|err| {
            let err: anyhow::Error = err.into();
            tracing::error!(
                error = err.as_ref() as &dyn std::error::Error,
                "gdb fatal error"
            );
            TargetError::Fatal(err)
        })
    }

    fn nonfatal(self) -> Result<T, TargetError<anyhow::Error>> {
        self.map_err(|err| {
            let err = err.into();
            tracing::warn!(
                error = err.as_ref() as &dyn std::error::Error,
                "gdb nonfatal error"
            );
            TargetError::Io(std::io::Error::new(std::io::ErrorKind::Other, err))
        })
    }
}

pub struct ArchError;

impl<E> From<ArchError> for TargetError<E> {
    fn from(_: ArchError) -> Self {
        TargetError::NonFatal
    }
}

/// Architecture-specific handling.
pub trait TargetArch:
    gdbstub::arch::Arch<Usize = Self::Address, BreakpointKind = usize> + Sized
{
    type Address: Copy + Into<u64> + TryFrom<u64>;

    /// Extract a single register.
    fn register(
        state: &DebuggerVpState,
        reg_id: Self::RegId,
        buf: &mut [u8],
    ) -> Result<usize, ArchError>;

    /// Extract the register file.
    fn registers(state: &DebuggerVpState, regs: &mut Self::Registers) -> Result<(), ArchError>;

    /// Update the register state from the register file.
    ///
    /// Returns false
    fn update_registers(
        state: &mut DebuggerVpState,
        regs: &Self::Registers,
    ) -> Result<(), ArchError>;

    /// Update a single register.
    fn update_register(
        state: &mut DebuggerVpState,
        reg_id: Self::RegId,
        val: &[u8],
    ) -> Result<(), ArchError>;

    /// Get the target description XML override implementation.
    fn support_target_description_xml_override<'a, 'b>(
        target: &'a mut VmTarget<'b, Self>,
    ) -> Option<TargetDescriptionXmlOverrideOps<'a, VmTarget<'b, Self>>> {
        let _ = target;
        None
    }
}

/// A [`VmProxy`] associated with a specific architecture `T`.
pub struct VmTarget<'a, T>(&'a mut VmProxy, PhantomData<T>);

impl<'a, T: TargetArch> VmTarget<'a, T> {
    pub fn new(vm_proxy: &'a mut VmProxy) -> Self {
        Self(vm_proxy, PhantomData)
    }
}

impl<T> Deref for VmTarget<'_, T> {
    type Target = VmProxy;

    fn deref(&self) -> &Self::Target {
        self.0
    }
}

impl<T> DerefMut for VmTarget<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0
    }
}

impl<T: TargetArch> Target for VmTarget<'_, T> {
    type Arch = T;
    type Error = anyhow::Error;

    // ExdiGdbSrv doesn't currently support RLE
    fn use_rle(&self) -> bool {
        false
    }

    #[inline(always)]
    fn base_ops(&mut self) -> gdbstub::target::ext::base::BaseOps<'_, Self::Arch, Self::Error> {
        gdbstub::target::ext::base::BaseOps::MultiThread(self)
    }

    #[inline(always)]
    fn support_target_description_xml_override(
        &mut self,
    ) -> Option<TargetDescriptionXmlOverrideOps<'_, Self>> {
        T::support_target_description_xml_override(self)
    }

    #[inline(always)]
    fn support_breakpoints(
        &mut self,
    ) -> Option<gdbstub::target::ext::breakpoints::BreakpointsOps<'_, Self>> {
        Some(self)
    }

    // We can rely on the GDB client overwrite the guest instruction stream when setting
    // software breakpoints. No need to reimplement that logic inside our stub.
    // NOTE: (8/20/2024) WinDbg's GDB client does not support this mode, and sents explicit sw breakpoint requests to the stub
    // NOTE: (8/20/2024) Does not work correctly when the paravisor is hosting the gdbstub (software breakpoints are not being trapped into VTL2)
    fn guard_rail_implicit_sw_breakpoints(&self) -> bool {
        true
    }
}
