// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::arith::ArithOp;
use super::AlignmentMode;
use super::Emulator;
use super::InternalError;
use crate::Cpu;
use iced_x86::Instruction;
use iced_x86::OpKind;
use iced_x86::Register;

/// Maximum number of repetitions that will be completed in a single run for rep prefixed instructions.
/// After this many iterations we'll return control to the processor to process any outstanding interrupts
/// or other events. However we won't increment RIP, so we'll be re-entered where we left off.
pub const MAX_REP_LOOPS: u64 = 1024;

/// State for rep ops. See [`Emulator::rep_op`].
struct RepState {
    pub count_reg: Register,
    pub done: u64,
    pub requested: u64,
    pub rep: Option<RepPrefix>,
    pub size: usize,
    pub delta: u64,
}

/// Repe and Repne contain the current status of the zero flag.
#[derive(Clone, Copy)]
enum RepPrefix {
    Rep,
    Repe(bool),
    Repne(bool),
}

impl RepState {
    fn update_zero(&mut self, new_zero: bool) {
        self.rep = match self.rep {
            Some(RepPrefix::Repe(_)) => Some(RepPrefix::Repe(new_zero)),
            Some(RepPrefix::Repne(_)) => Some(RepPrefix::Repne(new_zero)),
            Some(RepPrefix::Rep) => unreachable!(),
            None => None,
        }
    }

    fn check_done<E>(&self) -> Result<(), InternalError<E>> {
        if !self.is_done() {
            return Err(InternalError::Retry);
        }
        Ok(())
    }

    fn is_done(&self) -> bool {
        match self.rep {
            Some(RepPrefix::Repe(zero)) if !zero => return true,
            Some(RepPrefix::Repne(zero)) if zero => return true,
            _ => {}
        }

        self.done == self.requested
    }
}

/// Gets the RCX register of the appropriate size for the given `op_kind`.
fn sized_rcx(op_kind: OpKind) -> Register {
    match op_kind {
        OpKind::MemorySegSI | OpKind::MemorySegDI | OpKind::MemoryESDI => Register::CX,
        OpKind::MemorySegESI | OpKind::MemorySegEDI | OpKind::MemoryESEDI => Register::ECX,
        OpKind::MemorySegRSI | OpKind::MemorySegRDI | OpKind::MemoryESRDI => Register::RCX,
        _ => unreachable!(),
    }
}

/// Gets the RDI register of the appropriate size for the given `op_kind`.
fn sized_rdi(op_kind: OpKind) -> Register {
    match op_kind {
        OpKind::MemorySegDI | OpKind::MemoryESDI => Register::DI,
        OpKind::MemorySegEDI | OpKind::MemoryESEDI => Register::EDI,
        OpKind::MemorySegRDI | OpKind::MemoryESRDI => Register::RDI,
        _ => unreachable!(),
    }
}

/// Gets the RSI register of the appropriate size for the given `op_kind`.
fn sized_rsi(op_kind: OpKind) -> Register {
    match op_kind {
        OpKind::MemorySegSI => Register::SI,
        OpKind::MemorySegESI => Register::ESI,
        OpKind::MemorySegRSI => Register::RSI,
        _ => unreachable!(),
    }
}

impl<T: Cpu> Emulator<'_, T> {
    /// Generic function for handling the optional REP op for instructions.
    fn rep_op(
        &mut self,
        instr: &Instruction,
        op_kind: OpKind,
        is_cmps_scas: bool,
    ) -> Result<RepState, InternalError<T::Error>> {
        // iced doesn't provide us a good way to disambiguate between instructions that use REP and instructions that
        // use REPE (since they're the same byte), so we just pass along a bool from each function to help us here.
        let rep = match (
            is_cmps_scas,
            instr.has_rep_prefix(),
            instr.has_repne_prefix(),
        ) {
            (_, false, false) => None,
            (false, true, false) => Some(RepPrefix::Rep),
            // Testing on actual hardware shows that a REPNE prefix on a non-cmps/scas instruction is treated as a REP
            (false, false, true) => Some(RepPrefix::Rep),
            (true, true, false) => Some(RepPrefix::Repe(true)),
            (true, false, true) => Some(RepPrefix::Repne(false)),
            (_, true, true) => unreachable!(),
        };

        let count_reg = sized_rcx(op_kind);
        let requested = if rep.is_some() {
            self.cpu.gp(count_reg.into())
        } else {
            1
        };
        let size = instr.memory_size().size();
        let delta = if !self.cpu.rflags().direction() {
            size
        } else {
            size.wrapping_neg()
        };

        Ok(RepState {
            count_reg,
            rep,
            done: 0,
            requested,
            size,
            delta: delta as u64,
        })
    }

    fn rep_again(&mut self, rep_state: &mut RepState) -> bool {
        if rep_state.rep.is_some() {
            self.cpu.set_gp(
                rep_state.count_reg.into(),
                rep_state.requested - rep_state.done,
            );
        }
        if rep_state.is_done() || rep_state.done == MAX_REP_LOOPS {
            return false;
        }
        rep_state.done += 1;
        true
    }

    /// [rep] outs dx, seg:xsi
    ///
    /// Return true if instruction completed.
    pub(super) async fn outs(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op1_kind(), false)?;
        let rsi = sized_rsi(instr.op1_kind());
        while self.rep_again(&mut rep) {
            let data = &mut [0; 4][..rep.size];
            let offset = self.memory_op_offset(instr, 1);
            let io_register = self.cpu.gp(instr.op0_register().into()) as u16;

            self.read_memory(
                instr.memory_segment(),
                offset,
                AlignmentMode::Standard,
                data,
            )
            .await?;
            self.write_io(io_register, data).await?;

            self.cpu.set_gp(rsi.into(), offset.wrapping_add(rep.delta));
        }
        rep.check_done()?;
        Ok(())
    }

    /// [rep] ins es:xdi, dx
    ///
    /// Return true if instruction completed.
    pub(super) async fn ins(&mut self, instr: &Instruction) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op0_kind(), false)?;
        let rdi = sized_rdi(instr.op0_kind());
        while self.rep_again(&mut rep) {
            let offset = self.memory_op_offset(instr, 0);
            let io_register = self.cpu.gp(instr.op1_register().into()) as u16;

            let data = &mut [0; 4][..rep.size];
            self.read_io(io_register, data).await?;
            self.write_memory(Register::ES, offset, AlignmentMode::Standard, data)
                .await?;

            self.cpu.set_gp(rdi.into(), offset.wrapping_add(rep.delta));
        }
        rep.check_done()?;
        Ok(())
    }

    /// [rep] lods (r)ax, ds:xsi
    ///
    /// Return true if instruction completed.
    pub(super) async fn lods(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op1_kind(), false)?;
        let rsi = sized_rsi(instr.op1_kind());
        while self.rep_again(&mut rep) {
            let offset = self.memory_op_offset(instr, 1);
            let mut data = [0; 8];
            self.read_memory(
                instr.memory_segment(),
                offset,
                AlignmentMode::Standard,
                &mut data[..rep.size],
            )
            .await?;

            self.cpu
                .set_gp(instr.op0_register().into(), u64::from_le_bytes(data));
            self.cpu.set_gp(rsi.into(), offset.wrapping_add(rep.delta));
        }
        rep.check_done()?;
        Ok(())
    }

    /// [rep] stos es:xdi, (r)ax
    ///
    /// Return true if instruction completed.
    pub(super) async fn stos(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op0_kind(), false)?;
        let rdi = sized_rdi(instr.op0_kind());
        while self.rep_again(&mut rep) {
            let offset = self.memory_op_offset(instr, 0);
            let data = self.cpu.gp(instr.op1_register().into()).to_le_bytes();
            self.write_memory(
                Register::ES,
                offset,
                AlignmentMode::Standard,
                &data[..rep.size],
            )
            .await?;

            self.cpu.set_gp(rdi.into(), offset.wrapping_add(rep.delta));
        }
        rep.check_done()?;
        Ok(())
    }

    /// [rep] movs es:xdi seg:xsi
    ///
    /// Return true if instruction completed.
    pub(super) async fn movs(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op0_kind(), false)?;
        let rdi = sized_rdi(instr.op0_kind());
        let rsi = sized_rsi(instr.op1_kind());
        while self.rep_again(&mut rep) {
            let data = &mut [0; 8][..rep.size];

            let di_offset = self.memory_op_offset(instr, 0);
            let si_offset = self.memory_op_offset(instr, 1);

            self.read_memory(
                instr.memory_segment(),
                si_offset,
                AlignmentMode::Standard,
                data,
            )
            .await?;
            self.write_memory(Register::ES, di_offset, AlignmentMode::Standard, data)
                .await?;

            self.cpu
                .set_gp(rsi.into(), si_offset.wrapping_add(rep.delta));
            self.cpu
                .set_gp(rdi.into(), di_offset.wrapping_add(rep.delta));
        }
        rep.check_done()?;
        Ok(())
    }

    /// [rep] cmps es:xdi seg:xsi
    ///
    /// Return true if instruction completed.
    pub(super) async fn cmps(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op0_kind(), true)?;
        let rsi = sized_rsi(instr.op0_kind());
        let rdi = sized_rdi(instr.op1_kind());
        let mut left = 0;
        let mut right = 0;
        while self.rep_again(&mut rep) {
            let mut data_left = [0; 8];
            let mut data_right = [0; 8];

            let si_offset = self.memory_op_offset(instr, 0);
            let di_offset = self.memory_op_offset(instr, 1);

            self.read_memory(
                instr.memory_segment(),
                si_offset,
                AlignmentMode::Standard,
                &mut data_left[..rep.size],
            )
            .await?;
            self.read_memory(
                Register::ES,
                di_offset,
                AlignmentMode::Standard,
                &mut data_right[..rep.size],
            )
            .await?;

            left = u64::from_le_bytes(data_left);
            right = u64::from_le_bytes(data_right);
            rep.update_zero(left == right);

            self.cpu
                .set_gp(rsi.into(), si_offset.wrapping_add(rep.delta));
            self.cpu
                .set_gp(rdi.into(), di_offset.wrapping_add(rep.delta));
        }

        rep.check_done()?;
        if rep.requested != 0 {
            let mut rflags = self.cpu.rflags();
            let result = super::arith::CmpOp::op(left, right, rflags);
            super::arith::CmpOp::update_flags(&mut rflags, rep.size, result, left, right);
            self.cpu.set_rflags(rflags);
        }
        Ok(())
    }

    /// [rep] scas seg:xdi (r)ax
    ///
    /// Return true if instruction completed.
    pub(super) async fn scas(
        &mut self,
        instr: &Instruction,
    ) -> Result<(), InternalError<T::Error>> {
        let mut rep = self.rep_op(instr, instr.op1_kind(), true)?;
        let rax = self.cpu.gp(instr.op0_register().into());
        let rdi = sized_rdi(instr.op1_kind());
        let mut memval = 0;
        while self.rep_again(&mut rep) {
            let mut data = [0; 8];
            let di_offset = self.memory_op_offset(instr, 1);

            self.read_memory(
                Register::ES,
                di_offset,
                AlignmentMode::Standard,
                &mut data[..rep.size],
            )
            .await?;

            memval = u64::from_le_bytes(data);
            rep.update_zero(memval == rax);

            self.cpu
                .set_gp(rdi.into(), di_offset.wrapping_add(rep.delta));
        }

        rep.check_done()?;
        if rep.requested != 0 {
            let mut rflags = self.cpu.rflags();
            let result = super::arith::CmpOp::op(rax, memval, rflags);
            super::arith::CmpOp::update_flags(&mut rflags, rep.size, result, rax, memval);
            self.cpu.set_rflags(rflags);
        }
        Ok(())
    }
}
