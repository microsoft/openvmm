// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements an arm64 instruction emulator.

use crate::Cpu;
use crate::ReadKind;
use crate::opcodes::Aarch64DecodeGroup;
use crate::opcodes::Aarch64DecodeLoadStoreGroup;
use crate::opcodes::LoadRegisterLiteral;
use crate::opcodes::LoadStoreAtomic;
use crate::opcodes::LoadStoreRegister;
use crate::opcodes::LoadStoreRegisterPair;
use crate::opcodes::decode_group;
use aarch64defs::EsrEl2;
use inspect::Inspect;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error<E> {
    #[error("unknown instruction: {0:#x?}")]
    UnsupportedInstruction(u32),
    #[error("unsupported instruction group: {0:?} {1:#x?}")]
    UnsupportedInstructionGroup(Aarch64DecodeGroup, u32),
    #[error("unsupported load/store instruction: {0:?} {1:#x?}")]
    UnsupportedLoadStoreInstruction(Aarch64DecodeLoadStoreGroup, u32),
    #[error("unsupported instruction set (thumb)")]
    UnsupportedInstructionSet,
    #[error("instruction caused an exception")]
    InstructionException(EsrEl2, u64),
    #[error("memory access error - {1:?} @ {0:#x}")]
    MemoryAccess(u64, OperationKind, #[source] E),
}

#[derive(Debug, Default, Inspect)]
pub struct InterceptState {
    pub instruction_bytes: [u8; 4],
    pub instruction_byte_count: u8,
    pub gpa: Option<u64>,
    #[inspect(hex, with = "|&x| u64::from(x)")]
    pub syndrome: EsrEl2,
    pub interruption_pending: bool,
}

enum InternalError<E> {
    /// Report an error to the caller.
    Error(Box<Error<E>>),
}

impl<E> From<Error<E>> for InternalError<E> {
    fn from(err: Error<E>) -> Self {
        InternalError::Error(Box::new(err))
    }
}

impl<E> From<Box<Error<E>>> for InternalError<E> {
    fn from(err: Box<Error<E>>) -> Self {
        InternalError::Error(err)
    }
}

#[derive(Debug)]
pub(crate) struct EmulatorOperations<T: Cpu> {
    pub cpu: T,
    pub unprivileged: bool,
}

impl<T: Cpu> EmulatorOperations<T> {
    /// Reads an address base, checking SP alignment before applying any offset.
    pub fn base_address(&mut self, register: u8) -> Result<u64, Box<Error<T::Error>>> {
        if register != 31 {
            return Ok(self.cpu.x(register));
        }
        let address = self.cpu.sp();
        let sctlr = self.cpu.sctlr();
        let check_alignment = if self.cpu.cpsr().el() == 0 {
            sctlr.sa0()
        } else {
            sctlr.sa()
        };
        if check_alignment && !address.is_multiple_of(16) {
            return Err(Box::new(Error::InstructionException(
                EsrEl2::new()
                    .with_ec(aarch64defs::ExceptionClass::SP_ALIGNMENT_FAULT.0)
                    .with_il(true),
                address,
            )));
        }
        Ok(address)
    }

    /// Reads memory from the given guest VA, requiring write access for RMW operands.
    pub async fn read_memory(
        &mut self,
        gva: u64,
        data: &mut [u8],
        is_rmw: bool,
    ) -> Result<(), Box<Error<T::Error>>> {
        if self.cpu.sctlr().a() && !gva.is_multiple_of(data.len() as u64) {
            return Err(Box::new(Error::InstructionException(
                aarch64defs::IssDataAbort::new()
                    .with_dfsc(aarch64defs::FaultStatusCode::ALIGNMENT_FAULT)
                    .with_wnr(is_rmw)
                    .into(),
                gva,
            )));
        }
        let kind = if is_rmw {
            ReadKind::ReadModifyWrite
        } else if self.unprivileged {
            ReadKind::Unprivileged
        } else {
            ReadKind::Data
        };
        self.cpu.read_memory(gva, data, kind).await.map_err(|err| {
            Error::MemoryAccess(
                gva,
                if is_rmw {
                    OperationKind::Write
                } else {
                    OperationKind::Read
                },
                err,
            )
        })?;
        Ok(())
    }

    /// Writes memory to the given guest VA.
    pub async fn write_memory(
        &mut self,
        gva: u64,
        data: &[u8],
    ) -> Result<(), Box<Error<T::Error>>> {
        if self.cpu.sctlr().a() && !gva.is_multiple_of(data.len() as u64) {
            return Err(Box::new(Error::InstructionException(
                aarch64defs::IssDataAbort::new()
                    .with_dfsc(aarch64defs::FaultStatusCode::ALIGNMENT_FAULT)
                    .with_wnr(true)
                    .into(),
                gva,
            )));
        }
        self.cpu
            .write_memory(gva, data, self.unprivileged)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Write, err))?;
        Ok(())
    }
}

/// An instruction emulator.
#[derive(Debug)]
pub struct Emulator<'a, T: Cpu> {
    inner: EmulatorOperations<T>,
    intercept_state: &'a InterceptState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OperationKind {
    Read,
    Write,
    AddressComputation,
}

impl<'a, T: Cpu> Emulator<'a, T> {
    /// Creates new emulator with the given CPU and initial state.
    pub fn new(cpu: T, intercept_state: &'a InterceptState) -> Self {
        Emulator {
            inner: EmulatorOperations {
                cpu,
                unprivileged: false,
            },
            intercept_state,
        }
    }

    async fn decode_with_syndrome(&mut self) -> Result<bool, InternalError<T::Error>> {
        if !self.inner.cpu.supports_syndrome_decode() {
            return Ok(false);
        }
        let Some(gpa) = self.intercept_state.gpa else {
            return Ok(false);
        };
        let syndrome = self.intercept_state.syndrome;
        if !matches!(
            aarch64defs::ExceptionClass(syndrome.ec()),
            aarch64defs::ExceptionClass::DATA_ABORT | aarch64defs::ExceptionClass::DATA_ABORT_LOWER
        ) {
            return Ok(false);
        }

        let iss = aarch64defs::IssDataAbort::from(syndrome.iss());
        if !iss.isv() || !syndrome.il() {
            return Ok(false);
        }
        let len = 1 << iss.sas();
        if (gpa & 0xfff) + len as u64 > 0x1000 {
            return Ok(false);
        }
        let sign_extend = iss.sse();

        // Per "AArch64 System Register Descriptions/D23.2 General system control registers"
        // the SRT field is defined as
        //
        // > The register number of the Wt/Xt/Rt operand of the faulting
        // > instruction.
        //
        // In the A64 ISA TRM, Wt/Xt/Rt is used to designate the register number where the SP
        // register is not used whereas the addition of `|SP` tells that the SP register might
        // be used. Hence, the SRT field uses `0b11111` to encode `xzr`.
        //
        // Writing to `xzr` has no arch-observable effects, reading returns the all-zero's bit
        // pattern.
        let reg_index = iss.srt();
        if iss.wnr() {
            let data = match reg_index {
                0..=30 => self.inner.cpu.x(reg_index),
                31 => 0_u64,
                _ => unreachable!(),
            }
            .to_le_bytes();
            if iss.ar() {
                std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
            }
            self.inner
                .cpu
                .write_physical_memory(gpa, &data[..len])
                .await
                .map_err(|err| Error::MemoryAccess(gpa, OperationKind::Write, err))?;
        } else {
            let mut data = [0; 8];
            self.inner
                .cpu
                .read_physical_memory(gpa, &mut data[..len], false)
                .await
                .map_err(|err| Error::MemoryAccess(gpa, OperationKind::Read, err))?;
            if iss.ar() && reg_index != 31 {
                std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            }
            let mut data = u64::from_le_bytes(data);
            if sign_extend {
                let shift = 64 - len * 8;
                data = ((data as i64) << shift >> shift) as u64;
                if !iss.sf() {
                    data &= 0xffffffff;
                }
            }
            if reg_index != 31 {
                self.inner.cpu.update_x(reg_index, data);
            }
        }
        Ok(true)
    }

    pub async fn run(&mut self) -> Result<(), Box<Error<T::Error>>> {
        if self.inner.cpu.cpsr().aa32() {
            return Err(Box::new(Error::UnsupportedInstructionSet));
        }
        let pc = self.inner.cpu.pc();
        if !pc.is_multiple_of(4) {
            return Err(Box::new(Error::InstructionException(
                EsrEl2::new()
                    .with_ec(aarch64defs::ExceptionClass::PC_ALIGNMENT.0)
                    .with_il(true),
                pc,
            )));
        }
        match self.decode_with_syndrome().await {
            Ok(false) => (),
            Ok(true) => {
                self.inner.cpu.update_pc(pc.wrapping_add(4));
                return Ok(());
            }
            Err(InternalError::Error(err)) => return Err(err),
        };

        // If the intercept message did not include the instruction bytes, fetch them now.
        let instruction = if self.intercept_state.instruction_byte_count > 0 {
            if self.intercept_state.instruction_byte_count != 4 {
                return Err(Box::new(Error::UnsupportedInstructionSet));
            }
            u32::from_le_bytes(self.intercept_state.instruction_bytes)
        } else {
            let mut bytes = [0_u8; 4];
            let pc = self.inner.cpu.pc();
            self.inner
                .cpu
                .read_memory(pc, &mut bytes, ReadKind::Instruction)
                .await
                .map_err(|err| Error::MemoryAccess(pc, OperationKind::Read, err))?;
            u32::from_le_bytes(bytes)
        };
        let instruction_type = decode_group(instruction)?;
        match self.emulate(instruction, instruction_type).await {
            Ok(true) => {
                self.inner.cpu.update_pc(pc.wrapping_add(4));
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(InternalError::Error(err)) => Err(err),
        }
    }

    // DEVNOTE: The error type is boxed as a codesize optimization. See the comment on
    //          `run()` above for more information.
    /// Emulates the effects of an instruction.
    async fn emulate(
        &mut self,
        opcode: u32,
        instruction_type: Aarch64DecodeGroup,
    ) -> Result<bool, InternalError<T::Error>> {
        self.inner.unprivileged = matches!(
            instruction_type,
            Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterUnprivileged)
        );
        // We should not be emulating instructions that don't touch MMIO or PIO, even though we are capable of doing so.
        // If we are asked to do so it is usually indicative of some other problem, so abort so we can track that down.
        let result = match instruction_type {
            Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::UnscaledImmediate)
            | Aarch64DecodeGroup::LoadStore(
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            )
            | Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterUnprivileged)
            | Aarch64DecodeGroup::LoadStore(
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            )
            | Aarch64DecodeGroup::LoadStore(
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            )
            | Aarch64DecodeGroup::LoadStore(
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            )
            | Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterOffset) => {
                LoadStoreRegister(opcode).emulate(&mut self.inner).await
            }
            Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterLiteral) => {
                LoadRegisterLiteral(opcode).emulate(&mut self.inner).await
            }
            Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::NoAllocatePair)
            | Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex)
            | Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterPairOffset)
            | Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex) => {
                LoadStoreRegisterPair(opcode).emulate(&mut self.inner).await
            }
            Aarch64DecodeGroup::LoadStore(Aarch64DecodeLoadStoreGroup::Atomic) => {
                return LoadStoreAtomic(opcode)
                    .emulate(&mut self.inner)
                    .await
                    .map_err(Into::into);
            }
            Aarch64DecodeGroup::LoadStore(typ) => {
                return Err(InternalError::Error(Box::new(
                    Error::UnsupportedLoadStoreInstruction(typ, opcode),
                )));
            }
            group => {
                return Err(InternalError::Error(Box::new(
                    Error::UnsupportedInstructionGroup(group, opcode),
                )));
            }
        };
        result.map(|()| true).map_err(InternalError::Error)
    }
}
