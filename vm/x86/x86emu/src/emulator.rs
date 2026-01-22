// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements an x86 instruction emulator.

use crate::Cpu;
use crate::registers::Bitness;
use crate::registers::Gp;
use crate::registers::GpSize;
use crate::registers::RegisterIndex;
use crate::registers::Segment;
use crate::registers::bitness;
use iced_x86::Code;
use iced_x86::Decoder;
use iced_x86::DecoderError;
use iced_x86::DecoderOptions;
use iced_x86::Instruction;
use iced_x86::OpKind;
use iced_x86::Register;
use thiserror::Error;
use x86defs::Exception;

mod arith;
mod bt;
mod cmpxchg816;
mod cond;
pub mod fast_path;
mod instruction;
mod mov;
mod muldiv;
mod rep;
mod rflags;
mod shift_rotate;

pub use rep::MAX_REP_LOOPS;

// Trait to allow operating over u64 and u128 in some functions.
trait EmulatorRegister {
    // TODO: This really should be just a constant SIZE, but const generic support isn't good enough yet.
    type Array: std::ops::Index<std::ops::RangeTo<usize>, Output = [u8]>
        + std::ops::IndexMut<std::ops::RangeTo<usize>>;
    fn empty_bytes() -> Self::Array;
    fn from_le_bytes(bytes: Self::Array) -> Self;
    fn to_le_bytes(&self) -> Self::Array;
}
impl EmulatorRegister for u64 {
    type Array = [u8; 8];
    fn empty_bytes() -> Self::Array {
        [0; 8]
    }
    fn from_le_bytes(bytes: Self::Array) -> Self {
        Self::from_le_bytes(bytes)
    }
    fn to_le_bytes(&self) -> Self::Array {
        (*self).to_le_bytes()
    }
}
impl EmulatorRegister for u128 {
    type Array = [u8; 16];
    fn empty_bytes() -> Self::Array {
        [0; 16]
    }
    fn from_le_bytes(bytes: Self::Array) -> Self {
        Self::from_le_bytes(bytes)
    }
    fn to_le_bytes(&self) -> Self::Array {
        (*self).to_le_bytes()
    }
}

impl From<Register> for RegisterIndex {
    fn from(val: Register) -> Self {
        let size = match val.size() {
            1 => {
                if val >= Register::SPL || val < Register::AH {
                    GpSize::BYTE(0)
                } else {
                    GpSize::BYTE(8)
                }
            }
            2 => GpSize::WORD,
            4 => GpSize::DWORD,
            8 => GpSize::QWORD,
            _ => panic!("invalid gp register size"),
        };
        let extended_index = match val.full_register() {
            Register::RAX => Gp::RAX,
            Register::RCX => Gp::RCX,
            Register::RDX => Gp::RDX,
            Register::RBX => Gp::RBX,
            Register::RSP => Gp::RSP,
            Register::RBP => Gp::RBP,
            Register::RSI => Gp::RSI,
            Register::RDI => Gp::RDI,
            Register::R8 => Gp::R8,
            Register::R9 => Gp::R9,
            Register::R10 => Gp::R10,
            Register::R11 => Gp::R11,
            Register::R12 => Gp::R12,
            Register::R13 => Gp::R13,
            Register::R14 => Gp::R14,
            Register::R15 => Gp::R15,
            _ => panic!("invalid gp register index"),
        };
        RegisterIndex {
            extended_index,
            size,
        }
    }
}

impl From<Register> for Segment {
    fn from(val: Register) -> Self {
        match val {
            Register::ES => Segment::ES,
            Register::CS => Segment::CS,
            Register::SS => Segment::SS,
            Register::DS => Segment::DS,
            Register::FS => Segment::FS,
            Register::GS => Segment::GS,
            _ => panic!("invalid segment register index"),
        }
    }
}

/// An instruction emulator.
#[derive(Debug)]
pub struct Emulator<'a, T> {
    cpu: T,
    decoder_options: u32,
    bytes: &'a [u8],
}

#[derive(Debug, Error)]
pub enum Error<E> {
    #[error("asked to emulate an instruction that doesn't touch memory or PIO")]
    NonMemoryOrPortInstruction(Vec<u8>),
    #[error("unsupported instruction")]
    UnsupportedInstruction(Vec<u8>),
    #[error("memory access error - {1:?} @ {0:#x}")]
    MemoryAccess(u64, OperationKind, #[source] E),
    #[error("io port access error - {1:?} @ {0:#x}")]
    IoPort(u16, OperationKind, #[source] E),
    #[error("executing instruction caused exception due to {2:?} - {0:?}({1:?})")]
    InstructionException(Exception, Option<u32>, ExceptionCause),
    #[error("decode failure")]
    DecodeFailure,
    #[error("not enough instruction bytes")]
    NotEnoughBytes,
}

enum InternalError<E> {
    /// Return from the emulator without completing this instruction. Don't
    /// advance the RIP.
    Retry,
    /// Report an error to the caller.
    Error(Box<Error<E>>),
}

impl<E> From<Error<E>> for InternalError<E> {
    fn from(err: Error<E>) -> Self {
        InternalError::Error(Box::new(err))
    }
}

#[derive(Debug)]
pub enum ExceptionCause {
    MandatoryAlignment,
    AlignmentCheck,
    DebugTrap,
    DivideOverflow,
    DivideByZero,
    IoPrivilegeLevel,
    SegmentValidity,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OperationKind {
    Read,
    Write,
    AddressComputation,
}

#[derive(Copy, Clone)]
pub enum AlignmentMode {
    /// Always check that memory accesses are aligned to the given size. Generates a #GP exception if
    /// the check fails.
    Aligned(u64),
    /// No alignment checking performed, as the instruction supports unaligned accesses.
    Unaligned,
    /// If the alignment check flags are set, and we are in user mode, then check that memory accesses
    /// are aligned to their length. Generates an #AC exception if the check fails.
    Standard,
}

impl<'a, T: Cpu> Emulator<'a, T> {
    /// Creates new emulator with the given CPU and initial state.
    pub fn new(cpu: T, vendor: x86defs::cpuid::Vendor, bytes: &'a [u8]) -> Self {
        let mut decoder_options = 0;
        if vendor.is_amd_compatible() {
            decoder_options |= DecoderOptions::AMD;
        }
        Emulator {
            cpu,
            decoder_options,
            bytes,
        }
    }

    /// Gets the linear IP of the CPU, taking into account the code segment.
    ///
    /// Returns None if IP/EIP does not fit into the code segment.
    pub fn linear_ip(&mut self, offset: u64) -> Option<u64> {
        let rip = self.cpu.rip().wrapping_add(offset);
        let cr0 = self.cpu.cr0();
        let efer = self.cpu.efer();
        let cs = self.cpu.segment(Segment::CS);

        match bitness(cr0, efer, cs) {
            Bitness::Bit64 => Some(rip),
            Bitness::Bit32 | Bitness::Bit16 => {
                self.verify_segment_access(
                    Segment::CS,
                    OperationKind::AddressComputation,
                    offset,
                    1,
                )
                .ok()?;
                Some(cs.base.wrapping_add(rip))
            }
        }
    }

    /// Gets the current privilege level
    fn current_privilege_level(&mut self) -> u8 {
        self.cpu
            .segment(Segment::SS)
            .attributes
            .descriptor_privilege_level()
    }

    /// Gets whether the CPU was running in user mode before the emulator was invoked
    pub fn is_user_mode(&mut self) -> bool {
        self.current_privilege_level() == x86defs::USER_MODE_DPL
    }

    /// Gets the offset (relative to the segment) for a memory operand.
    fn memory_op_offset(&mut self, instr: &Instruction, operand: u32) -> u64 {
        instruction::memory_op_offset(&mut self.cpu, instr, operand)
    }

    /// Computes the linear GVA from the segment:offset. Also validates that the
    /// access within the segment is allowed.
    fn compute_and_validate_gva(
        &mut self,
        segment: Segment,
        offset: u64,
        len: usize,
        op: OperationKind,
        alignment: AlignmentMode,
    ) -> Result<u64, Error<T::Error>> {
        let cr0 = self.cpu.cr0();
        let efer = self.cpu.efer();
        let cs = self.cpu.segment(Segment::CS);

        let base = match bitness(cr0, efer, cs) {
            Bitness::Bit64 => {
                if matches!(segment, Segment::FS | Segment::GS) {
                    self.cpu.segment(segment).base
                } else {
                    0
                }
            }
            Bitness::Bit32 | Bitness::Bit16 => {
                self.verify_segment_access(segment, op, offset, len)?;
                self.cpu.segment(segment).base
            }
        };

        let gva = base.wrapping_add(offset);
        tracing::trace!(?op, base, offset, gva, "compute_gva");
        self.verify_gva_alignment(gva, len, alignment)?;
        Ok(gva)
    }

    /// Validates that the provided operation is valid within the provided segment. Returns the appropriate
    /// exception if it is not. This method should not be called when operating in long mode, as all of its
    /// checks are ignored in this case.
    fn verify_segment_access(
        &mut self,
        segment: Segment,
        op: OperationKind,
        offset: u64,
        len: usize,
    ) -> Result<(), Error<T::Error>> {
        // All of these conditions are ignored for 64-bit mode, this method should not be called.
        let cr0 = self.cpu.cr0();
        let efer = self.cpu.efer();
        let cs = self.cpu.segment(Segment::CS);
        let bitness = bitness(cr0, efer, cs);
        assert_ne!(bitness, Bitness::Bit64);

        let segment_value = self.cpu.segment(segment);

        // Since we're not in long mode, offset can be at most u32::MAX, and same goes for len. So this
        // can't overflow.
        let offset_end = offset + len as u64 - 1;

        let gp0 = Error::InstructionException(
            Exception::GENERAL_PROTECTION_FAULT,
            Some(0),
            ExceptionCause::SegmentValidity,
        );
        let gpindex = Error::InstructionException(
            Exception::GENERAL_PROTECTION_FAULT,
            Some(segment_value.selector.into()),
            ExceptionCause::SegmentValidity,
        );

        // CS is treated differently from data segments. The segment type is treated as a code segment, and
        // writes are forbidden in protected mode. It also can't be expand-down and will always be present.
        if matches!(segment, Segment::CS) {
            // Forbid writes in protected mode
            if bitness == Bitness::Bit32 && op == OperationKind::Write {
                return Err(gp0);
            }

            // If we're reading, check for the segment not being readable
            if op == OperationKind::Read && segment_value.attributes.segment_type() & 0b0010 == 0 {
                return Err(gp0);
            }

            // Check that the offset and length are not outside the segment limits
            if offset_end > segment_value.limit as u64 {
                return Err(gp0);
            }
        } else {
            // Check for the segment not being present
            if !segment_value.attributes.present() {
                Err(Error::InstructionException(
                    Exception::SEGMENT_NOT_PRESENT,
                    None,
                    ExceptionCause::SegmentValidity,
                ))?;
            }

            // Check for a null selector, ignoring the RPL
            if bitness == Bitness::Bit32 && segment_value.selector & !0x3 == 0 {
                return Err(gp0);
            }

            // Check the RPL (if 32-bit) and CPL against the DPL
            let rpl = if matches!(bitness, Bitness::Bit32) {
                (segment_value.selector & 0x3) as u8
            } else {
                0
            };
            let cpl = self.current_privilege_level();
            let dpl = segment_value.attributes.descriptor_privilege_level();
            if rpl > dpl || cpl > dpl {
                return Err(gpindex);
            }

            // Check for the segment not being a data segment
            if !(segment_value.attributes.non_system_segment()
                && segment_value.attributes.segment_type() & 0b1000 == 0)
            {
                return Err(gpindex);
            }

            // If we're writing, check for the segment not being writable
            if op == OperationKind::Write && segment_value.attributes.segment_type() & 0b0010 == 0 {
                return Err(gp0);
            }

            // Check that the offset and length are not outside the segment limits
            if segment_value.attributes.segment_type() & 0b0100 == 0 {
                // Segment is not expand-down
                if offset_end > segment_value.limit as u64 {
                    return Err(gp0);
                }
            } else {
                // Segment is expand-down
                let max = if segment_value.attributes.default() {
                    u32::MAX as u64
                } else {
                    u16::MAX as u64
                };
                if offset <= segment_value.limit as u64 || offset_end > max {
                    return Err(gp0);
                }
            };
        }

        Ok(())
    }

    /// Validates that the provided gva is valid for the provided alignment mode. Returns the appropriate
    /// exception if it is not.
    fn verify_gva_alignment(
        &mut self,
        gva: u64,
        len: usize,
        alignment: AlignmentMode,
    ) -> Result<(), Error<T::Error>> {
        match alignment {
            AlignmentMode::Aligned(a) => {
                if !gva.is_multiple_of(a) {
                    Err(Error::InstructionException(
                        Exception::GENERAL_PROTECTION_FAULT,
                        Some(0),
                        ExceptionCause::MandatoryAlignment,
                    ))?
                }
            }
            AlignmentMode::Unaligned => {}
            AlignmentMode::Standard => {
                if self.is_user_mode()
                    && self.cpu.rflags().alignment_check()
                    && self.cpu.cr0() & x86defs::X64_CR0_AM != 0
                {
                    if !gva.is_multiple_of(len as u64) {
                        Err(Error::InstructionException(
                            Exception::ALIGNMENT_CHECK,
                            None,
                            ExceptionCause::AlignmentCheck,
                        ))?
                    }
                }
            }
        }
        Ok(())
    }

    /// Reads memory from the given segment:offset.
    pub async fn read_memory(
        &mut self,
        segment: Segment,
        offset: u64,
        alignment: AlignmentMode,
        data: &mut [u8],
    ) -> Result<(), Error<T::Error>> {
        let gva = self.compute_and_validate_gva(
            segment,
            offset,
            data.len(),
            OperationKind::Read,
            alignment,
        )?;
        let user_mode = self.is_user_mode();
        self.cpu
            .read_memory(gva, data, user_mode)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Read, err))?;

        Ok(())
    }

    /// Writes memory to the given segment:offset.
    pub async fn write_memory(
        &mut self,
        segment: Segment,
        offset: u64,
        alignment: AlignmentMode,
        data: &[u8],
    ) -> Result<(), Error<T::Error>> {
        let gva = self.compute_and_validate_gva(
            segment,
            offset,
            data.len(),
            OperationKind::Write,
            alignment,
        )?;
        let cpl = self.is_user_mode();
        self.cpu
            .write_memory(gva, data, cpl)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Write, err))?;

        Ok(())
    }

    /// Writes memory to the given segment:offset, validating that the current
    /// memory value matches `current`.
    ///
    /// Returns `true` if the memory was written, `false` if there was a
    /// mismatch.
    async fn compare_and_write_memory(
        &mut self,
        segment: Segment,
        offset: u64,
        alignment: AlignmentMode,
        current: &[u8],
        new: &[u8],
    ) -> Result<bool, InternalError<T::Error>> {
        assert_eq!(current.len(), new.len());
        let user_mode = self.is_user_mode();
        let gva = self.compute_and_validate_gva(
            segment,
            offset,
            new.len(),
            OperationKind::Write,
            alignment,
        )?;
        let success = self
            .cpu
            .compare_and_write_memory(gva, current, new, user_mode)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Write, err))?;

        Ok(success)
    }

    /// Reads a value from a memory operand.
    async fn read_memory_op<R: EmulatorRegister>(
        &mut self,
        instr: &Instruction,
        operand: u32,
        alignment: AlignmentMode,
    ) -> Result<R, InternalError<T::Error>> {
        let offset = self.memory_op_offset(instr, operand);
        let mut data = R::empty_bytes();
        self.read_memory(
            instr.memory_segment().into(),
            offset,
            alignment,
            &mut data[..instr.memory_size().size()],
        )
        .await?;
        Ok(R::from_le_bytes(data))
    }

    /// Write a value to a memory operand.
    async fn write_memory_op<R: EmulatorRegister>(
        &mut self,
        instr: &Instruction,
        operand: u32,
        alignment: AlignmentMode,
        data: R,
    ) -> Result<(), InternalError<T::Error>> {
        let offset = self.memory_op_offset(instr, operand);
        self.write_memory(
            instr.memory_segment().into(),
            offset,
            alignment,
            &data.to_le_bytes()[..instr.memory_size().size()],
        )
        .await?;
        Ok(())
    }

    /// Write a value to a memory operand, validating that the current value in
    /// memory matches `current` if the instruction has an explicit or implicit
    /// lock prefix.
    ///
    /// If it does not match, return `Err(InternalError::Retry)` to
    /// retry the emulation (after an opportunity for the caller to pause or
    /// abort the emulation).
    async fn compare_if_locked_and_write_memory_op<R: EmulatorRegister>(
        &mut self,
        instr: &Instruction,
        operand: u32,
        alignment: AlignmentMode,
        current: R,
        new: R,
    ) -> Result<(), InternalError<T::Error>> {
        let offset = self.memory_op_offset(instr, operand);
        // xchg is implicitly locked.
        if instr.has_lock_prefix() || instr.mnemonic() == iced_x86::Mnemonic::Xchg {
            if !self
                .compare_and_write_memory(
                    instr.memory_segment().into(),
                    offset,
                    alignment,
                    &current.to_le_bytes()[..instr.memory_size().size()],
                    &new.to_le_bytes()[..instr.memory_size().size()],
                )
                .await?
            {
                return Err(InternalError::Retry);
            }
        } else {
            self.write_memory(
                instr.memory_segment().into(),
                offset,
                alignment,
                &new.to_le_bytes()[..instr.memory_size().size()],
            )
            .await?;
        }
        Ok(())
    }

    /// Checks that the current privilege level has access to port I/O.
    /// We do not currently support TSS-based I/O privileges.
    fn check_io_privilege_level(&mut self) -> Result<(), InternalError<T::Error>> {
        if self.current_privilege_level() > self.cpu.rflags().io_privilege_level() {
            Err(Error::InstructionException(
                Exception::GENERAL_PROTECTION_FAULT,
                Some(0),
                ExceptionCause::IoPrivilegeLevel,
            ))?;
        }
        Ok(())
    }

    /// Reads from the given port.
    async fn read_io(&mut self, port: u16, data: &mut [u8]) -> Result<(), InternalError<T::Error>> {
        self.check_io_privilege_level()?;
        self.cpu
            .read_io(port, data)
            .await
            .map_err(|err| Error::IoPort(port, OperationKind::Read, err))?;

        Ok(())
    }

    /// Writes to the given port.
    async fn write_io(&mut self, port: u16, data: &[u8]) -> Result<(), InternalError<T::Error>> {
        self.check_io_privilege_level()?;
        self.cpu
            .write_io(port, data)
            .await
            .map_err(|err| Error::IoPort(port, OperationKind::Write, err))?;

        Ok(())
    }

    /// Gets a value for a given operand.
    async fn op_value(
        &mut self,
        instr: &Instruction,
        operand: u32,
    ) -> Result<u64, InternalError<T::Error>> {
        Ok(match instr.op_kind(operand) {
            OpKind::Memory => {
                self.read_memory_op(instr, operand, AlignmentMode::Standard)
                    .await?
            }
            OpKind::Register => self.cpu.gp(instr.op_register(operand).into()),
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate64
            | OpKind::Immediate8to16
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate32to64 => instr.immediate(operand),
            _ => Err(self.unsupported_instruction(instr))?,
        })
    }

    /// Gets a value for a given operand with sign extension.
    async fn op_value_sign_extend(
        &mut self,
        instr: &Instruction,
        operand: u32,
    ) -> Result<i64, InternalError<T::Error>> {
        let value = self.op_value(instr, operand).await?;
        let size = instr.memory_size().size();
        let shift_size = 64 - (size * 8);
        let new_value = ((value as i64) << shift_size) >> shift_size;
        Ok(new_value)
    }

    /// Write a value to operand 0.
    async fn write_op_0(
        &mut self,
        instr: &Instruction,
        value: u64,
    ) -> Result<(), InternalError<T::Error>> {
        match instr.op0_kind() {
            OpKind::Memory => {
                self.write_memory_op(instr, 0, AlignmentMode::Standard, value)
                    .await?
            }
            OpKind::Register => {
                self.cpu.set_gp(instr.op0_register().into(), value);
            }
            _ => Err(self.unsupported_instruction(instr))?,
        };
        Ok(())
    }

    /// Write a value to operand 0, validating that the current value matches
    /// `current` if the instruction has an explicit or implicit lock prefix and
    /// operand 0 is a memory location.
    ///
    /// If it does not match, return `Err(InternalError::Retry)` to
    /// retry the emulation (after an opportunity for the caller to pause or
    /// abort the emulation).
    async fn compare_if_locked_and_write_op_0(
        &mut self,
        instr: &Instruction,
        current: u64,
        new: u64,
    ) -> Result<(), InternalError<T::Error>> {
        match instr.op0_kind() {
            OpKind::Memory => {
                self.compare_if_locked_and_write_memory_op(
                    instr,
                    0,
                    AlignmentMode::Standard,
                    current,
                    new,
                )
                .await?
            }
            OpKind::Register => {
                self.cpu.set_gp(instr.op0_register().into(), new);
            }
            _ => Err(self.unsupported_instruction(instr))?,
        };
        Ok(())
    }

    // DEVNOTE: The error type is boxed so as to shrink the overall size of the return value.
    //          Since errors are expected to occur far less frequently than `Ok(())`, this saves
    //          us from copying a larger value into the future on every success for a codesize win.
    /// Emulates a single instruction.
    pub async fn run(&mut self) -> Result<(), Box<Error<T::Error>>> {
        let cr0 = self.cpu.cr0();
        let efer = self.cpu.efer();
        let cs = self.cpu.segment(Segment::CS);
        let bitness = bitness(cr0, efer, cs);
        let mut decoder = Decoder::new(bitness.into(), self.bytes, self.decoder_options);
        decoder.set_ip(self.cpu.rip());
        let instr = decoder.decode();
        if instr.code() == Code::INVALID {
            match decoder.last_error() {
                DecoderError::None => unreachable!(),
                DecoderError::NoMoreBytes => return Err(Box::new(Error::NotEnoughBytes)),
                err => {
                    tracing::warn!(
                        error = ?err,
                        bytes = ?self.bytes,
                        "could not decode instruction"
                    );
                    return Err(Box::new(Error::DecodeFailure));
                }
            }
        }
        tracing::trace!(
            bytes = ?self.bytes[..instr.len()],
            cs = ?self.cpu.segment(Segment::CS),
            rip = self.cpu.rip(),
            ?bitness,
            "Emulating instruction",
        );
        match self.emulate(&instr).await {
            // If `Retry` is returned, then the RIP has not been advanced, but
            // some register and memory state may have changed. The processor is
            // in a consistent, observable state. The caller should resume
            // emulation from the same RIP, possibly after running the VM to
            // poll for interrupts or whatever.
            Ok(()) | Err(InternalError::Retry) => {}
            Err(InternalError::Error(err)) => return Err(err),
        }
        Ok(())
    }

    // DEVNOTE: The error type is boxed as a codesize optimization. See the comment on
    //          `run()` above for more information.
    /// Emulates the effects of an instruction.
    async fn emulate(&mut self, instr: &Instruction) -> Result<(), InternalError<T::Error>> {
        // We should not be emulating instructions that don't touch MMIO or PIO, even though we are capable of doing so.
        // If we are asked to do so it is usually indicative of some other problem, so abort so we can track that down.
        if !instr.op_kinds().any(|x| x == OpKind::Memory) && !instr.is_string_instruction() {
            Err(Error::NonMemoryOrPortInstruction(
                self.bytes[..instr.len()].into(),
            ))?;
        }

        match instr.code() {
            // mov r/m, r
            // mov r, r/m
            // mov ar, moffs
            // mov moffs, ar
            // mov rm, imm
            // movzx
            // movdiri
            // movnti
            Code::Mov_rm8_r8
            | Code::Mov_rm16_r16
            | Code::Mov_rm32_r32
            | Code::Mov_rm64_r64
            | Code::Mov_r8_rm8
            | Code::Mov_r16_rm16
            | Code::Mov_r32_rm32
            | Code::Mov_r64_rm64
            | Code::Mov_AL_moffs8
            | Code::Mov_AX_moffs16
            | Code::Mov_EAX_moffs32
            | Code::Mov_RAX_moffs64
            | Code::Mov_moffs8_AL
            | Code::Mov_moffs16_AX
            | Code::Mov_moffs32_EAX
            | Code::Mov_moffs64_RAX
            | Code::Mov_rm8_imm8
            | Code::Mov_rm16_imm16
            | Code::Mov_rm32_imm32
            | Code::Mov_rm64_imm32
            | Code::Movzx_r16_rm8
            | Code::Movzx_r32_rm8
            | Code::Movzx_r64_rm8
            | Code::Movzx_r16_rm16
            | Code::Movzx_r32_rm16
            | Code::Movzx_r64_rm16
            | Code::Movdiri_m32_r32
            | Code::Movdiri_m64_r64
            | Code::Movnti_m32_r32
            | Code::Movnti_m64_r64 => self.mov(instr).await,

            // movsx
            // movsxd
            Code::Movsx_r16_rm8
            | Code::Movsx_r32_rm8
            | Code::Movsx_r64_rm8
            | Code::Movsx_r16_rm16
            | Code::Movsx_r32_rm16
            | Code::Movsx_r64_rm16
            | Code::Movsxd_r16_rm16
            | Code::Movsxd_r32_rm32
            | Code::Movsxd_r64_rm32 => self.movsx(instr).await,

            // movups
            // movupd
            // movdqu
            // movntdq
            // movntps
            // movntpd
            Code::Movups_xmm_xmmm128
            | Code::Movups_xmmm128_xmm
            | Code::Movupd_xmm_xmmm128
            | Code::Movupd_xmmm128_xmm
            | Code::Movdqu_xmm_xmmm128
            | Code::Movdqu_xmmm128_xmm
            | Code::Movntdq_m128_xmm
            | Code::Movntps_m128_xmm
            | Code::Movntpd_m128_xmm => self.mov_sse(instr, AlignmentMode::Unaligned).await,

            // movaps
            // movapd
            // movdqa
            Code::Movaps_xmm_xmmm128
            | Code::Movaps_xmmm128_xmm
            | Code::Movapd_xmm_xmmm128
            | Code::Movapd_xmmm128_xmm
            | Code::Movdqa_xmm_xmmm128
            | Code::Movdqa_xmmm128_xmm => self.mov_sse(instr, AlignmentMode::Aligned(16)).await,

            Code::Movdir64b_r16_m512 | Code::Movdir64b_r32_m512 | Code::Movdir64b_r64_m512 => {
                self.movdir64b(instr).await
            }

            // movs
            Code::Movsb_m8_m8 | Code::Movsw_m16_m16 | Code::Movsd_m32_m32 | Code::Movsq_m64_m64 => {
                self.movs(instr).await
            }

            // cmp
            Code::Cmp_r64_rm64
            | Code::Cmp_r32_rm32
            | Code::Cmp_r16_rm16
            | Code::Cmp_r8_rm8
            | Code::Cmp_rm64_r64
            | Code::Cmp_rm32_r32
            | Code::Cmp_rm16_r16
            | Code::Cmp_rm8_r8
            | Code::Cmp_rm64_imm32
            | Code::Cmp_rm64_imm8
            | Code::Cmp_rm32_imm32
            | Code::Cmp_rm32_imm8
            | Code::Cmp_rm16_imm16
            | Code::Cmp_rm16_imm8
            | Code::Cmp_rm8_imm8 => self.arith::<arith::CmpOp>(instr).await,

            // xchg
            Code::Xchg_rm8_r8 | Code::Xchg_rm16_r16 | Code::Xchg_rm32_r32 | Code::Xchg_rm64_r64 => {
                self.xchg(instr).await
            }

            // cmpxchg
            Code::Cmpxchg_rm8_r8
            | Code::Cmpxchg_rm16_r16
            | Code::Cmpxchg_rm32_r32
            | Code::Cmpxchg_rm64_r64 => self.cmpxchg(instr).await,

            // test
            Code::Test_rm64_r64
            | Code::Test_rm32_r32
            | Code::Test_rm16_r16
            | Code::Test_rm8_r8
            | Code::Test_rm64_imm32
            | Code::Test_rm32_imm32
            | Code::Test_rm16_imm16
            | Code::Test_rm8_imm8 => self.arith::<arith::TestOp>(instr).await,

            // and
            Code::And_r64_rm64
            | Code::And_r32_rm32
            | Code::And_r16_rm16
            | Code::And_r8_rm8
            | Code::And_rm64_r64
            | Code::And_rm32_r32
            | Code::And_rm16_r16
            | Code::And_rm8_r8
            | Code::And_rm64_imm32
            | Code::And_rm64_imm8
            | Code::And_rm32_imm32
            | Code::And_rm32_imm8
            | Code::And_rm16_imm16
            | Code::And_rm16_imm8
            | Code::And_rm8_imm8 => self.arith::<arith::AndOp>(instr).await,

            // add
            Code::Add_r64_rm64
            | Code::Add_r32_rm32
            | Code::Add_r16_rm16
            | Code::Add_r8_rm8
            | Code::Add_rm64_r64
            | Code::Add_rm32_r32
            | Code::Add_rm16_r16
            | Code::Add_rm8_r8
            | Code::Add_rm64_imm32
            | Code::Add_rm64_imm8
            | Code::Add_rm32_imm32
            | Code::Add_rm32_imm8
            | Code::Add_rm16_imm16
            | Code::Add_rm16_imm8
            | Code::Add_rm8_imm8 => self.arith::<arith::AddOp>(instr).await,

            // adc
            Code::Adc_r64_rm64
            | Code::Adc_r32_rm32
            | Code::Adc_r16_rm16
            | Code::Adc_r8_rm8
            | Code::Adc_rm64_r64
            | Code::Adc_rm32_r32
            | Code::Adc_rm16_r16
            | Code::Adc_rm8_r8
            | Code::Adc_rm64_imm32
            | Code::Adc_rm64_imm8
            | Code::Adc_rm32_imm32
            | Code::Adc_rm32_imm8
            | Code::Adc_rm16_imm16
            | Code::Adc_rm16_imm8
            | Code::Adc_rm8_imm8 => self.arith::<arith::AdcOp>(instr).await,

            Code::Xadd_rm8_r8 | Code::Xadd_rm16_r16 | Code::Xadd_rm32_r32 | Code::Xadd_rm64_r64 => {
                self.xadd(instr).await
            }

            // sub
            Code::Sub_r64_rm64
            | Code::Sub_r32_rm32
            | Code::Sub_r16_rm16
            | Code::Sub_r8_rm8
            | Code::Sub_rm64_r64
            | Code::Sub_rm32_r32
            | Code::Sub_rm16_r16
            | Code::Sub_rm8_r8
            | Code::Sub_rm64_imm32
            | Code::Sub_rm64_imm8
            | Code::Sub_rm32_imm32
            | Code::Sub_rm32_imm8
            | Code::Sub_rm16_imm16
            | Code::Sub_rm16_imm8
            | Code::Sub_rm8_imm8 => self.arith::<arith::SubOp>(instr).await,

            // sbb
            Code::Sbb_r64_rm64
            | Code::Sbb_r32_rm32
            | Code::Sbb_r16_rm16
            | Code::Sbb_r8_rm8
            | Code::Sbb_rm64_r64
            | Code::Sbb_rm32_r32
            | Code::Sbb_rm16_r16
            | Code::Sbb_rm8_r8
            | Code::Sbb_rm64_imm32
            | Code::Sbb_rm64_imm8
            | Code::Sbb_rm32_imm32
            | Code::Sbb_rm32_imm8
            | Code::Sbb_rm16_imm16
            | Code::Sbb_rm16_imm8
            | Code::Sbb_rm8_imm8 => self.arith::<arith::SbbOp>(instr).await,

            // or
            Code::Or_r64_rm64
            | Code::Or_r32_rm32
            | Code::Or_r16_rm16
            | Code::Or_r8_rm8
            | Code::Or_rm64_r64
            | Code::Or_rm32_r32
            | Code::Or_rm16_r16
            | Code::Or_rm8_r8
            | Code::Or_rm64_imm32
            | Code::Or_rm64_imm8
            | Code::Or_rm32_imm32
            | Code::Or_rm32_imm8
            | Code::Or_rm16_imm16
            | Code::Or_rm16_imm8
            | Code::Or_rm8_imm8 => self.arith::<arith::OrOp>(instr).await,

            // xor
            Code::Xor_r64_rm64
            | Code::Xor_r32_rm32
            | Code::Xor_r16_rm16
            | Code::Xor_r8_rm8
            | Code::Xor_rm64_r64
            | Code::Xor_rm32_r32
            | Code::Xor_rm16_r16
            | Code::Xor_rm8_r8
            | Code::Xor_rm64_imm32
            | Code::Xor_rm64_imm8
            | Code::Xor_rm32_imm32
            | Code::Xor_rm32_imm8
            | Code::Xor_rm16_imm16
            | Code::Xor_rm16_imm8
            | Code::Xor_rm8_imm8 => self.arith::<arith::XorOp>(instr).await,

            // neg
            Code::Neg_rm8 | Code::Neg_rm16 | Code::Neg_rm32 | Code::Neg_rm64 => {
                self.unary_arith::<arith::NegOp>(instr).await
            }

            // not
            Code::Not_rm8 | Code::Not_rm16 | Code::Not_rm32 | Code::Not_rm64 => {
                self.unary_arith::<arith::NotOp>(instr).await
            }

            // mul
            Code::Mul_rm8 | Code::Mul_rm16 | Code::Mul_rm32 | Code::Mul_rm64 => {
                self.unary_mul(instr).await
            }

            // imul rm
            Code::Imul_rm8 | Code::Imul_rm16 | Code::Imul_rm32 | Code::Imul_rm64 => {
                self.unary_imul(instr).await
            }

            // imul r rm (imm)
            Code::Imul_r16_rm16
            | Code::Imul_r32_rm32
            | Code::Imul_r64_rm64
            | Code::Imul_r16_rm16_imm8
            | Code::Imul_r16_rm16_imm16
            | Code::Imul_r32_rm32_imm8
            | Code::Imul_r32_rm32_imm32
            | Code::Imul_r64_rm64_imm8
            | Code::Imul_r64_rm64_imm32 => self.imul(instr).await,

            // div
            Code::Div_rm8 | Code::Div_rm16 | Code::Div_rm32 | Code::Div_rm64 => {
                self.unary_div(instr).await
            }

            // idiv
            Code::Idiv_rm8 | Code::Idiv_rm16 | Code::Idiv_rm32 | Code::Idiv_rm64 => {
                self.unary_idiv(instr).await
            }

            // shl
            Code::Shl_rm8_1
            | Code::Shl_rm8_CL
            | Code::Shl_rm8_imm8
            | Code::Shl_rm16_1
            | Code::Shl_rm16_CL
            | Code::Shl_rm16_imm8
            | Code::Shl_rm32_1
            | Code::Shl_rm32_CL
            | Code::Shl_rm32_imm8
            | Code::Shl_rm64_1
            | Code::Shl_rm64_CL
            | Code::Shl_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::SxlOp>(instr)
                    .await
            }

            // shr
            Code::Shr_rm8_1
            | Code::Shr_rm8_CL
            | Code::Shr_rm8_imm8
            | Code::Shr_rm16_1
            | Code::Shr_rm16_CL
            | Code::Shr_rm16_imm8
            | Code::Shr_rm32_1
            | Code::Shr_rm32_CL
            | Code::Shr_rm32_imm8
            | Code::Shr_rm64_1
            | Code::Shr_rm64_CL
            | Code::Shr_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::ShrOp>(instr)
                    .await
            }

            // sar
            Code::Sar_rm8_1
            | Code::Sar_rm8_CL
            | Code::Sar_rm8_imm8
            | Code::Sar_rm16_1
            | Code::Sar_rm16_CL
            | Code::Sar_rm16_imm8
            | Code::Sar_rm32_1
            | Code::Sar_rm32_CL
            | Code::Sar_rm32_imm8
            | Code::Sar_rm64_1
            | Code::Sar_rm64_CL
            | Code::Sar_rm64_imm8 => self.shift_arithmetic_right(instr).await,

            // sal
            Code::Sal_rm8_1
            | Code::Sal_rm8_CL
            | Code::Sal_rm8_imm8
            | Code::Sal_rm16_1
            | Code::Sal_rm16_CL
            | Code::Sal_rm16_imm8
            | Code::Sal_rm32_1
            | Code::Sal_rm32_CL
            | Code::Sal_rm32_imm8
            | Code::Sal_rm64_1
            | Code::Sal_rm64_CL
            | Code::Sal_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::SxlOp>(instr)
                    .await
            }

            Code::Shld_rm16_r16_CL
            | Code::Shld_rm16_r16_imm8
            | Code::Shld_rm32_r32_CL
            | Code::Shld_rm32_r32_imm8
            | Code::Shld_rm64_r64_CL
            | Code::Shld_rm64_r64_imm8 => self.shld(instr).await,

            Code::Shrd_rm16_r16_CL
            | Code::Shrd_rm16_r16_imm8
            | Code::Shrd_rm32_r32_CL
            | Code::Shrd_rm32_r32_imm8
            | Code::Shrd_rm64_r64_CL
            | Code::Shrd_rm64_r64_imm8 => self.shrd(instr).await,

            // rcl
            Code::Rcl_rm8_1
            | Code::Rcl_rm8_CL
            | Code::Rcl_rm8_imm8
            | Code::Rcl_rm16_1
            | Code::Rcl_rm16_CL
            | Code::Rcl_rm16_imm8
            | Code::Rcl_rm32_1
            | Code::Rcl_rm32_CL
            | Code::Rcl_rm32_imm8
            | Code::Rcl_rm64_1
            | Code::Rcl_rm64_CL
            | Code::Rcl_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::RclOp>(instr)
                    .await
            }

            // rcr
            Code::Rcr_rm8_1
            | Code::Rcr_rm8_CL
            | Code::Rcr_rm8_imm8
            | Code::Rcr_rm16_1
            | Code::Rcr_rm16_CL
            | Code::Rcr_rm16_imm8
            | Code::Rcr_rm32_1
            | Code::Rcr_rm32_CL
            | Code::Rcr_rm32_imm8
            | Code::Rcr_rm64_1
            | Code::Rcr_rm64_CL
            | Code::Rcr_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::RcrOp>(instr)
                    .await
            }

            // rol
            Code::Rol_rm8_1
            | Code::Rol_rm8_CL
            | Code::Rol_rm8_imm8
            | Code::Rol_rm16_1
            | Code::Rol_rm16_CL
            | Code::Rol_rm16_imm8
            | Code::Rol_rm32_1
            | Code::Rol_rm32_CL
            | Code::Rol_rm32_imm8
            | Code::Rol_rm64_1
            | Code::Rol_rm64_CL
            | Code::Rol_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::RolOp>(instr)
                    .await
            }

            // ror
            Code::Ror_rm8_1
            | Code::Ror_rm8_CL
            | Code::Ror_rm8_imm8
            | Code::Ror_rm16_1
            | Code::Ror_rm16_CL
            | Code::Ror_rm16_imm8
            | Code::Ror_rm32_1
            | Code::Ror_rm32_CL
            | Code::Ror_rm32_imm8
            | Code::Ror_rm64_1
            | Code::Ror_rm64_CL
            | Code::Ror_rm64_imm8 => {
                self.shift_sign_unextended::<shift_rotate::RorOp>(instr)
                    .await
            }

            // outs
            Code::Outsb_DX_m8 | Code::Outsw_DX_m16 | Code::Outsd_DX_m32 => self.outs(instr).await,

            // ins
            Code::Insb_m8_DX | Code::Insw_m16_DX | Code::Insd_m32_DX => self.ins(instr).await,

            // lods
            Code::Lodsb_AL_m8 | Code::Lodsw_AX_m16 | Code::Lodsd_EAX_m32 | Code::Lodsq_RAX_m64 => {
                self.lods(instr).await
            }

            // stos
            Code::Stosb_m8_AL | Code::Stosw_m16_AX | Code::Stosd_m32_EAX | Code::Stosq_m64_RAX => {
                self.stos(instr).await
            }

            // cmps
            Code::Cmpsb_m8_m8 | Code::Cmpsw_m16_m16 | Code::Cmpsd_m32_m32 | Code::Cmpsq_m64_m64 => {
                self.cmps(instr).await
            }

            // scas
            Code::Scasb_AL_m8 | Code::Scasw_AX_m16 | Code::Scasd_EAX_m32 | Code::Scasq_RAX_m64 => {
                self.scas(instr).await
            }

            // bt/bts/btr/btc
            Code::Bt_rm16_imm8
            | Code::Bt_rm32_imm8
            | Code::Bt_rm64_imm8
            | Code::Bt_rm16_r16
            | Code::Bt_rm32_r32
            | Code::Bt_rm64_r64 => self.bt_m::<bt::TestOp>(instr).await,
            Code::Bts_rm16_imm8
            | Code::Bts_rm32_imm8
            | Code::Bts_rm64_imm8
            | Code::Bts_rm16_r16
            | Code::Bts_rm32_r32
            | Code::Bts_rm64_r64 => self.bt_m::<bt::SetOp>(instr).await,
            Code::Btr_rm16_imm8
            | Code::Btr_rm32_imm8
            | Code::Btr_rm64_imm8
            | Code::Btr_rm16_r16
            | Code::Btr_rm32_r32
            | Code::Btr_rm64_r64 => self.bt_m::<bt::ResetOp>(instr).await,
            Code::Btc_rm16_imm8
            | Code::Btc_rm32_imm8
            | Code::Btc_rm64_imm8
            | Code::Btc_rm16_r16
            | Code::Btc_rm32_r32
            | Code::Btc_rm64_r64 => self.bt_m::<bt::ComplementOp>(instr).await,

            // inc/dec
            Code::Inc_rm8 | Code::Inc_rm16 | Code::Inc_rm32 | Code::Inc_rm64 => {
                self.unary_arith::<arith::IncOp>(instr).await
            }
            Code::Dec_rm8 | Code::Dec_rm16 | Code::Dec_rm32 | Code::Dec_rm64 => {
                self.unary_arith::<arith::DecOp>(instr).await
            }

            // set*
            Code::Seta_rm8
            | Code::Setae_rm8
            | Code::Setb_rm8
            | Code::Setbe_rm8
            | Code::Sete_rm8
            | Code::Setg_rm8
            | Code::Setge_rm8
            | Code::Setl_rm8
            | Code::Setle_rm8
            | Code::Setne_rm8
            | Code::Setno_rm8
            | Code::Setnp_rm8
            | Code::Setns_rm8
            | Code::Seto_rm8
            | Code::Setp_rm8
            | Code::Sets_rm8 => self.setcc(instr).await,

            // cmov*
            Code::Cmova_r16_rm16
            | Code::Cmova_r32_rm32
            | Code::Cmova_r64_rm64
            | Code::Cmovae_r16_rm16
            | Code::Cmovae_r32_rm32
            | Code::Cmovae_r64_rm64
            | Code::Cmovb_r16_rm16
            | Code::Cmovb_r32_rm32
            | Code::Cmovb_r64_rm64
            | Code::Cmovbe_r16_rm16
            | Code::Cmovbe_r32_rm32
            | Code::Cmovbe_r64_rm64
            | Code::Cmove_r16_rm16
            | Code::Cmove_r32_rm32
            | Code::Cmove_r64_rm64
            | Code::Cmovg_r16_rm16
            | Code::Cmovg_r32_rm32
            | Code::Cmovg_r64_rm64
            | Code::Cmovge_r16_rm16
            | Code::Cmovge_r32_rm32
            | Code::Cmovge_r64_rm64
            | Code::Cmovl_r16_rm16
            | Code::Cmovl_r32_rm32
            | Code::Cmovl_r64_rm64
            | Code::Cmovle_r16_rm16
            | Code::Cmovle_r32_rm32
            | Code::Cmovle_r64_rm64
            | Code::Cmovne_r16_rm16
            | Code::Cmovne_r32_rm32
            | Code::Cmovne_r64_rm64
            | Code::Cmovno_r16_rm16
            | Code::Cmovno_r32_rm32
            | Code::Cmovno_r64_rm64
            | Code::Cmovnp_r16_rm16
            | Code::Cmovnp_r32_rm32
            | Code::Cmovnp_r64_rm64
            | Code::Cmovns_r16_rm16
            | Code::Cmovns_r32_rm32
            | Code::Cmovns_r64_rm64
            | Code::Cmovo_r16_rm16
            | Code::Cmovo_r32_rm32
            | Code::Cmovo_r64_rm64
            | Code::Cmovp_r16_rm16
            | Code::Cmovp_r32_rm32
            | Code::Cmovp_r64_rm64
            | Code::Cmovs_r16_rm16
            | Code::Cmovs_r32_rm32
            | Code::Cmovs_r64_rm64 => self.cmovcc(instr).await,

            Code::Cmpxchg8b_m64 | Code::Cmpxchg16b_m128 => self.cmpxchg8_16(instr).await,

            // in/out are explicitly unsupported, as they should be handled by the fast path in
            // virt_support_x86emu::emulate::emulate_io instead.
            Code::In_AL_imm8
            | Code::In_AX_imm8
            | Code::In_EAX_imm8
            | Code::In_AL_DX
            | Code::In_AX_DX
            | Code::In_EAX_DX
            | Code::Out_imm8_AL
            | Code::Out_imm8_AX
            | Code::Out_imm8_EAX
            | Code::Out_DX_AL
            | Code::Out_DX_AX
            | Code::Out_DX_EAX
            | _ => Err(self.unsupported_instruction(instr).into()),
        }?;

        // The instruction is complete. Update the RIP and check for traps.
        self.cpu.set_rip(instr.next_ip());
        let mut rflags = self.cpu.rflags();
        if rflags.trap() {
            rflags.set_trap(false);
            self.cpu.set_rflags(rflags);
            return Err(Error::InstructionException(
                Exception::DEBUG,
                None,
                ExceptionCause::DebugTrap,
            ))?;
        }

        Ok(())
    }

    fn unsupported_instruction(&self, instr: &Instruction) -> Error<T::Error> {
        Error::UnsupportedInstruction(self.bytes[..instr.len()].into())
    }
}
