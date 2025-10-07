// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::TargetArch;
use super::VmTarget;
use crate::gdb::VmProxy;
use gdbstub::target;
use gdbstub::target::TargetError;
use gdbstub::target::TargetResult;
use gdbstub::target::ext::breakpoints::WatchKind;
use vmm_core_defs::debug_rpc::BreakpointSize;
use vmm_core_defs::debug_rpc::BreakpointType;
use vmm_core_defs::debug_rpc::HardwareBreakpoint;

impl<T: TargetArch> target::ext::breakpoints::Breakpoints for VmTarget<'_, T> {
    #[inline(always)]
    fn support_sw_breakpoint(
        &mut self,
    ) -> Option<target::ext::breakpoints::SwBreakpointOps<'_, Self>> {
        None
    }

    #[inline(always)]
    fn support_hw_breakpoint(
        &mut self,
    ) -> Option<target::ext::breakpoints::HwBreakpointOps<'_, Self>> {
        Some(self)
    }

    #[inline(always)]
    fn support_hw_watchpoint(
        &mut self,
    ) -> Option<target::ext::breakpoints::HwWatchpointOps<'_, Self>> {
        Some(self)
    }
}

impl<T: TargetArch> target::ext::breakpoints::SwBreakpoint for VmTarget<'_, T> {
    fn add_sw_breakpoint(&mut self, addr: T::Usize, instr_len: usize) -> TargetResult<bool, Self> {
        tracing::error!(
            "setting sw breakpoint at {:#018x?} with len {}",
            addr.into(),
            instr_len
        );

        tracing::error!("sw breakpoints are not implemented!");
        Err(TargetError::NonFatal)
    }

    fn remove_sw_breakpoint(
        &mut self,
        addr: T::Usize,
        instr_len: usize,
    ) -> TargetResult<bool, Self> {
        tracing::error!(
            "removing sw breakpoint at {:#018x?} with len {}",
            addr.into(),
            instr_len
        );

        tracing::error!("sw breakpoints are not implemented!");
        Err(TargetError::NonFatal)
    }
}

impl VmProxy {
    fn add_breakpoint(&mut self, bp: HardwareBreakpoint) -> bool {
        if let Some(slot) = self.breakpoints.iter_mut().find(|x| x.is_none()) {
            *slot = Some(bp);
            true
        } else {
            false
        }
    }

    fn remove_breakpoint(&mut self, bp: HardwareBreakpoint) -> bool {
        if let Some(slot) = self
            .breakpoints
            .iter_mut()
            .find(|x| x.as_ref() == Some(&bp))
        {
            *slot = None;
            true
        } else {
            false
        }
    }
}

impl<T: TargetArch> target::ext::breakpoints::HwBreakpoint for VmTarget<'_, T> {
    fn add_hw_breakpoint(
        &mut self,
        addr: T::Usize,
        _kind: T::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        Ok(self.add_breakpoint(HardwareBreakpoint {
            address: addr.into(),
            ty: BreakpointType::Execute,
            size: BreakpointSize::Byte,
        }))
    }

    fn remove_hw_breakpoint(
        &mut self,
        addr: T::Usize,
        _kind: T::BreakpointKind,
    ) -> TargetResult<bool, Self> {
        Ok(self.remove_breakpoint(HardwareBreakpoint {
            address: addr.into(),
            ty: BreakpointType::Execute,
            size: BreakpointSize::Byte,
        }))
    }
}

impl<T: TargetArch> target::ext::breakpoints::HwWatchpoint for VmTarget<'_, T> {
    fn add_hw_watchpoint(
        &mut self,
        addr: T::Usize,
        len: T::Usize,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        Ok(self.add_breakpoint(HardwareBreakpoint {
            address: addr.into(),
            ty: type_from_watch_kind(kind),
            size: (len.into() as usize)
                .try_into()
                .map_err(|_| TargetError::NonFatal)?,
        }))
    }

    fn remove_hw_watchpoint(
        &mut self,
        addr: T::Usize,
        len: T::Usize,
        kind: WatchKind,
    ) -> TargetResult<bool, Self> {
        Ok(self.remove_breakpoint(HardwareBreakpoint {
            address: addr.into(),
            ty: type_from_watch_kind(kind),
            size: (len.into() as usize)
                .try_into()
                .map_err(|_| TargetError::NonFatal)?,
        }))
    }
}

fn type_from_watch_kind(kind: WatchKind) -> BreakpointType {
    match kind {
        WatchKind::Write => BreakpointType::Write,
        WatchKind::Read | WatchKind::ReadWrite => BreakpointType::ReadOrWrite,
    }
}
