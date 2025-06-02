// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements an arm64 instruction emulator.

use crate::Cpu;
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
}

impl<T: Cpu> EmulatorOperations<T> {
    /// Reads an instruction to execute from the given guest VA.
    pub async fn read_instruction(
        &mut self,
        gva: u64,
        data: &mut [u8],
    ) -> Result<(), Box<Error<T::Error>>> {
        self.cpu
            .read_instruction(gva, data)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Read, err))?;
        Ok(())
    }

    /// Reads memory from the given guest VA.
    pub async fn read_memory(
        &mut self,
        gva: u64,
        data: &mut [u8],
    ) -> Result<(), Box<Error<T::Error>>> {
        self.cpu
            .read_memory(gva, data)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Read, err))?;
        Ok(())
    }

    /// Reads memory from the given guest PA.
    pub async fn read_physical_memory(
        &mut self,
        gpa: u64,
        data: &mut [u8],
    ) -> Result<(), Box<Error<T::Error>>> {
        self.cpu
            .read_physical_memory(gpa, data)
            .await
            .map_err(|err| Error::MemoryAccess(gpa, OperationKind::Read, err))?;
        Ok(())
    }

    /// Writes memory to the given guest VA.
    pub async fn write_memory(
        &mut self,
        gva: u64,
        data: &[u8],
    ) -> Result<(), Box<Error<T::Error>>> {
        self.cpu
            .write_memory(gva, data)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Write, err))?;
        Ok(())
    }

    /// Writes memory to the given guest PA.
    pub async fn write_physical_memory(
        &mut self,
        gpa: u64,
        data: &[u8],
    ) -> Result<(), Box<Error<T::Error>>> {
        self.cpu
            .write_physical_memory(gpa, data)
            .await
            .map_err(|err| Error::MemoryAccess(gpa, OperationKind::Write, err))?;
        Ok(())
    }

    /// Writes memory to the given guest VA if the current value matches.
    pub async fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
    ) -> Result<bool, Box<Error<T::Error>>> {
        let mut success = false;
        self.cpu
            .compare_and_write_memory(gva, current, new, &mut success)
            .await
            .map_err(|err| Error::MemoryAccess(gva, OperationKind::Write, err))?;
        Ok(success)
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
            inner: EmulatorOperations { cpu },
            intercept_state,
        }
    }

    fn advance_pc(&mut self, count: u64) {
        let new_pc = self.inner.cpu.pc().wrapping_add(count);
        self.inner.cpu.update_pc(new_pc);
    }

    async fn decode_with_syndrome(&mut self) -> Result<bool, InternalError<T::Error>> {
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
        if !iss.isv() {
            return Ok(false);
        }
        let len = 1 << iss.sas();
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
            .to_ne_bytes();
            self.inner.write_physical_memory(gpa, &data[..len]).await?;
        } else if reg_index != 31 {
            let mut data = [0; 8];
            // tracing::info!(gpa, len = data.len(), "reading memory from syndrome decode");
            self.inner
                .read_physical_memory(gpa, &mut data[..len])
                .await?;
            let mut data = u64::from_ne_bytes(data);
            if sign_extend {
                let shift = 64 - len * 8;
                data = ((data as i64) << shift >> shift) as u64;
                if !iss.sf() {
                    data &= 0xffffffff;
                }
            }
            self.inner.cpu.update_x(reg_index, data);
        }
        self.advance_pc(if syndrome.il() { 4 } else { 2 });
        Ok(true)
    }

    pub async fn run(&mut self) -> Result<(), Box<Error<T::Error>>> {
        match self.decode_with_syndrome().await {
            Ok(false) => (),
            Ok(true) => return Ok(()),
            Err(InternalError::Error(err)) => {
                tracing::error!(%err, "Error decoding access via syndrome");
            }
        };

        // If the intercept message did not include the instruction bytes, fetch them now.
        let instruction = if self.intercept_state.instruction_byte_count > 0 {
            if self.intercept_state.instruction_byte_count != 4 {
                return Err(Box::new(Error::UnsupportedInstructionSet));
            }
            u32::from_ne_bytes(self.intercept_state.instruction_bytes)
        } else {
            let mut bytes = [0_u8; 4];
            let pc = self.inner.cpu.pc();
            self.inner.read_instruction(pc, &mut bytes[..]).await?;
            u32::from_ne_bytes(bytes)
        };
        let instruction_type = decode_group(instruction)?;
        match self.emulate(instruction, instruction_type).await {
            Ok(()) => {
                self.advance_pc(4);
                Ok(())
            }
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
    ) -> Result<(), InternalError<T::Error>> {
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
                LoadStoreAtomic(opcode).emulate(&mut self.inner).await
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
        result.map_err(InternalError::Error)
    }
}
