// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Structs to parse aarch64 instructions.

use crate::emulator::EmulatorOperations;
use crate::emulator::Error;
use crate::Cpu;

#[derive(Debug, PartialEq)]
pub enum Aarch64DecodeGroup {
    Reserved,
    ScalableMatrixExtension,
    ScalableVectorExtension,
    ImmediateData,
    BranchesExceptionsAndSystem,
    LoadStore(Aarch64DecodeLoadStoreGroup),
    RegisterData,
    FpAndAdvancedData,
}

pub fn decode_group<E>(opcode: u32) -> Result<Aarch64DecodeGroup, Box<Error<E>>> {
    let op0 = opcode >> 31;
    let op1 = ((opcode >> 25) as u8) & 0xf;
    Ok(match op1 {
        0 if op0 == 0 => Aarch64DecodeGroup::Reserved,
        0 => Aarch64DecodeGroup::ScalableMatrixExtension,
        2 => Aarch64DecodeGroup::ScalableVectorExtension,
        8 | 9 => Aarch64DecodeGroup::ImmediateData,
        10 | 11 => Aarch64DecodeGroup::BranchesExceptionsAndSystem,
        4 | 6 | 12 | 14 => Aarch64DecodeGroup::LoadStore(decode_load_store_group(opcode)?),
        5 | 13 => Aarch64DecodeGroup::RegisterData,
        7 | 15 => Aarch64DecodeGroup::FpAndAdvancedData,
        1 | 3 => {
            return Err(Box::new(Error::UnsupportedInstruction(opcode)));
        }
        16.. => unreachable!("masked with 0xf"),
    })
}

#[derive(Debug, PartialEq)]
pub enum Aarch64DecodeLoadStoreGroup {
    CompareAndSwapPair,
    AdvancedSimdMultiStruct,
    AdvancedSimdMultiStructPostIndex,
    AdvancedSimd,
    AdvancedSimdPostIndex,
    MemoryTags,
    ExclusivePair,
    ExclusiveRegister,
    Ordered,
    CompareAndSwap,
    UnscaledImmediate,
    RegisterLiteral,
    MemoryCopyAndSet,
    NoAllocatePair,
    RegisterPairPostIndex,
    RegisterPairOffset,
    RegisterPairPreIndex,
    RegisterUnscaledImmediate,
    RegisterImmediatePostIndex,
    RegisterUnprivileged,
    RegisterImmediatePreIndex,
    Atomic,
    RegisterOffset,
    RegisterPac,
    RegisterUnsignedImmediate,
}

fn decode_load_store_group<E>(opcode: u32) -> Result<Aarch64DecodeLoadStoreGroup, Box<Error<E>>> {
    let op0 = (opcode >> 28) as u8;
    let op1 = (opcode >> 26) & 1;
    let op2 = ((opcode >> 23) & 3) as u8;
    let op3 = ((opcode >> 16) & 0x3f) as u8;
    let op4 = ((opcode >> 10) & 3) as u8;
    Ok(match op0 {
        0 | 4 | 8 | 12 if op1 == 0 => {
            if op2 == 0 {
                if (op3 & 0x20) == 0 {
                    Aarch64DecodeLoadStoreGroup::ExclusiveRegister
                } else if op0 == 0 || op0 == 4 {
                    Aarch64DecodeLoadStoreGroup::CompareAndSwapPair
                } else {
                    Aarch64DecodeLoadStoreGroup::ExclusivePair
                }
            } else if (op3 & 0x20) != 0 {
                Aarch64DecodeLoadStoreGroup::CompareAndSwap
            } else {
                Aarch64DecodeLoadStoreGroup::Ordered
            }
        }
        0 | 4 => match op2 {
            0 if op3 == 0 => Aarch64DecodeLoadStoreGroup::AdvancedSimdMultiStruct,
            0 => {
                return Err(Box::new(Error::UnsupportedInstruction(opcode)));
            }
            1 if ((op3 & 0x20) == 0) => {
                Aarch64DecodeLoadStoreGroup::AdvancedSimdMultiStructPostIndex
            }
            1 => {
                return Err(Box::new(Error::UnsupportedInstruction(opcode)));
            }
            2 if ((op3 & 0x1f) == 0) => Aarch64DecodeLoadStoreGroup::AdvancedSimd,
            2 => {
                return Err(Box::new(Error::UnsupportedInstruction(opcode)));
            }
            3 => Aarch64DecodeLoadStoreGroup::AdvancedSimdPostIndex,
            4.. => unreachable!("masked with 0x3"),
        },
        8 | 12 => {
            return Err(Box::new(Error::UnsupportedInstruction(opcode)));
        }
        13 if op1 == 0 && (op2 & 2) != 0 && (op3 & 0x20) != 0 => {
            Aarch64DecodeLoadStoreGroup::MemoryTags
        }
        1 | 5 | 9 | 13 if (op2 & 2) != 0 => {
            if (op0 == 13) && (op3 & 0x20) != 0 {
                Aarch64DecodeLoadStoreGroup::MemoryTags
            } else if (op3 & 0x20) != 0 {
                return Err(Box::new(Error::UnsupportedInstruction(opcode)));
            } else if op4 == 0 {
                Aarch64DecodeLoadStoreGroup::UnscaledImmediate
            } else {
                Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet
            }
        }
        1 | 5 | 9 | 13 => Aarch64DecodeLoadStoreGroup::RegisterLiteral,
        2 | 6 | 10 | 14 if op2 == 0 => Aarch64DecodeLoadStoreGroup::NoAllocatePair,
        2 | 6 | 10 | 14 if op2 == 1 => Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex,
        2 | 6 | 10 | 14 if op2 == 2 => Aarch64DecodeLoadStoreGroup::RegisterPairOffset,
        2 | 6 | 10 | 14 => Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex,
        3 | 7 | 11 | 15 if ((op2 & 2) != 0) => {
            Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate
        }
        3 | 7 | 11 | 15 if op4 == 0 => {
            if (op3 & 0x20) != 0 {
                Aarch64DecodeLoadStoreGroup::Atomic
            } else {
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate
            }
        }
        3 | 7 | 11 | 15 if op4 == 1 => {
            if (op3 & 0x20) != 0 {
                Aarch64DecodeLoadStoreGroup::RegisterPac
            } else {
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex
            }
        }
        3 | 7 | 11 | 15 if op4 == 2 => {
            if (op3 & 0x20) != 0 {
                Aarch64DecodeLoadStoreGroup::RegisterOffset
            } else {
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged
            }
        }
        3 | 7 | 11 | 15 => {
            if (op3 & 0x20) != 0 {
                Aarch64DecodeLoadStoreGroup::RegisterPac
            } else {
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex
            }
        }
        16.. => unreachable!("masked with 0xf"),
    })
}

pub struct LoadRegisterLiteral(pub u32);
impl LoadRegisterLiteral {
    fn opc(&self) -> u8 {
        (self.0 >> 30) as u8
    }

    // register name: V
    fn is_fp_register(&self) -> bool {
        ((self.0 >> 26) & 1) != 0
    }

    fn imm(&self) -> i64 {
        // imm = imm_val * 4
        let imm = (self.0 >> 3) & 0x1ffffc;
        // sign-extend
        let imm = if imm & 0x100000 != 0 {
            0xffe00000 | imm
        } else {
            imm
        };
        imm as i32 as i64
    }

    fn rt(&self) -> u8 {
        (self.0 & 0x1f) as u8
    }

    pub async fn emulate<T: Cpu>(
        &self,
        emulate: &mut EmulatorOperations<T>,
    ) -> Result<(), Box<Error<T::Error>>> {
        if self.opc() == 3 && !self.is_fp_register() {
            // PRFM prefetch instruction
            return Ok(());
        }
        let address = emulate.cpu.pc().wrapping_add(self.imm() as u64);
        let mut buf = [0_u8; 16];
        let new_val = if self.opc() == 0 {
            // 32-bit load
            emulate.read_memory(address, &mut buf[..4]).await?;
            u32::from_ne_bytes(buf[..4].try_into().unwrap()) as u128
        } else if self.opc() == 1 {
            // 64-bit load
            emulate.read_memory(address, &mut buf[..8]).await?;
            u64::from_ne_bytes(buf[..8].try_into().unwrap()) as u128
        } else if self.opc() == 2 && !self.is_fp_register() {
            // 32-bit sign extend load
            emulate.read_memory(address, &mut buf[..4]).await?;
            let original = u32::from_ne_bytes(buf[..4].try_into().unwrap());
            if original & 0x80000000 != 0 {
                0xffffffff_00000000 | original as u128
            } else {
                original as u128
            }
        } else if self.opc() == 2 {
            // 128-bit floating point load
            emulate.read_memory(address, &mut buf[..]).await?;
            u128::from_ne_bytes(buf[..].try_into().unwrap())
        } else {
            return Err(Box::new(Error::UnsupportedInstruction(self.0)));
        };
        if !self.is_fp_register() {
            if self.rt() != 31 {
                emulate.cpu.update_x(self.rt(), new_val as u64);
            }
        } else {
            emulate.cpu.update_q(self.rt(), new_val);
        }
        Ok(())
    }
}

#[derive(Debug)]
enum LoadStoreRegisterByteCount {
    One,
    Two,
    Four,
    Eight,
    FloatingPoint,
}

pub struct LoadStoreRegister(pub u32);
impl LoadStoreRegister {
    fn size(&self) -> u8 {
        (self.0 >> 30) as u8
    }

    // register name: V
    fn is_fp_register(&self) -> bool {
        (self.0 & 0x04000000) != 0
    }

    fn opc(&self) -> u8 {
        ((self.0 >> 22) & 3) as u8
    }

    fn data_size<E>(&self) -> Result<LoadStoreRegisterByteCount, Error<E>> {
        let op = self.size() << 2 | self.opc();
        let result = match op {
            0..=1 => LoadStoreRegisterByteCount::One,
            2..=3 if self.is_fp_register() => LoadStoreRegisterByteCount::FloatingPoint,
            2..=3 => LoadStoreRegisterByteCount::One,
            4..=7 => LoadStoreRegisterByteCount::Two,
            8..=9 => LoadStoreRegisterByteCount::Four,
            10 if !self.is_fp_register() => LoadStoreRegisterByteCount::Four,
            12..=13 => LoadStoreRegisterByteCount::Eight,
            _ => {
                return Err(Error::UnsupportedInstruction(self.0));
            }
        };
        Ok(result)
    }

    fn address<T: Cpu>(
        &self,
        emulate: &mut EmulatorOperations<T>,
    ) -> Result<(u64, u64), Box<Error<T::Error>>> {
        let start_address = if self.rn() < 31 {
            emulate.cpu.x(self.rn())
        } else {
            emulate.cpu.sp()
        };
        let end_address = match decode_load_store_group(self.0)? {
            Aarch64DecodeLoadStoreGroup::UnscaledImmediate
            | Aarch64DecodeLoadStoreGroup::RegisterUnprivileged
            | Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate
            | Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex
            | Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex => {
                // imm9
                let unsigned_addr = (self.0 >> 12) & 0x1ff;
                let signed_addr = if (unsigned_addr & 0x100) != 0 {
                    (0xfffffe00 | unsigned_addr) as i32
                } else {
                    unsigned_addr as i32
                } as i64;
                (start_address as i64).wrapping_add(signed_addr) as u64
            }
            Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate => {
                let size = self.data_size()?;
                let shift = 10
                    - match size {
                        LoadStoreRegisterByteCount::One => 0,
                        LoadStoreRegisterByteCount::Two => 1,
                        LoadStoreRegisterByteCount::Four => 2,
                        LoadStoreRegisterByteCount::Eight => 3,
                        LoadStoreRegisterByteCount::FloatingPoint => 4,
                    };
                start_address.wrapping_add((self.0 as u64 & 0x3ffc00) >> shift)
            }
            Aarch64DecodeLoadStoreGroup::RegisterOffset => {
                let rm = ((self.0 >> 16) & 0x1f) as u8;
                let option = (self.0 >> 13) & 0x7;
                let apply_shift = (self.0 & 0x1000) != 0;
                let offset = if rm == 31 { 0 } else { emulate.cpu.x(rm) };
                let offset = if (option & 1) != 0 {
                    // 64-bit register index
                    offset
                } else if option == 6 && (offset & 0x80000000) != 0 {
                    // SXTW: 32-bit sign-extended
                    0xffffffff_00000000 | (offset & 0xffffffff)
                } else {
                    // UXTW or SXTW w/o the sign bit set: 32-bit unsigned
                    offset & 0xffffffff
                };
                // If shift (S bit) is set, offset is in size being fetched instead of bytes.
                let offset = if apply_shift {
                    let size = self.data_size()?;
                    match size {
                        LoadStoreRegisterByteCount::One => {
                            return Err(Box::new(Error::UnsupportedInstruction(self.0)))
                        }
                        LoadStoreRegisterByteCount::Two => offset << 1,
                        LoadStoreRegisterByteCount::Four => offset << 2,
                        LoadStoreRegisterByteCount::Eight => offset << 3,
                        LoadStoreRegisterByteCount::FloatingPoint => offset << 4,
                    }
                } else {
                    offset
                };
                start_address.wrapping_add(offset)
            }
            _ => panic!("Unsupported opcode"),
        };
        Ok((start_address, end_address))
    }

    fn rn(&self) -> u8 {
        ((self.0 >> 5) & 0x1f) as u8
    }

    fn rt(&self) -> u8 {
        (self.0 & 0x1f) as u8
    }

    pub async fn emulate<T: Cpu>(
        &self,
        emulate: &mut EmulatorOperations<T>,
    ) -> Result<(), Box<Error<T::Error>>> {
        let op_group = decode_load_store_group(self.0)?;
        let op = self.size() << 2 | self.opc();
        if op == 14 {
            match op_group {
                Aarch64DecodeLoadStoreGroup::UnscaledImmediate
                | Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate
                | Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate
                | Aarch64DecodeLoadStoreGroup::RegisterOffset => {
                    // ignore prefetch instructions
                    return Ok(());
                }
                _ => {
                    // unallocated
                    return Err(Box::new(Error::UnsupportedInstruction(self.0)));
                }
            }
        }
        let (start_address, end_address) = self.address(emulate)?;
        let address = if matches!(
            op_group,
            Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex
        ) {
            start_address
        } else {
            end_address
        };
        let size = self.data_size()?;
        let register_index = self.rt();
        tracing::info!(
            ?op_group,
            op,
            start_address,
            end_address,
            address,
            ?size,
            "instruction emulation"
        );
        // index register: self.rn()
        // src/dest register: self.rt()
        // self.v() == 1: FP registers
        // self.size() << 2 | self.opc
        //     0 // 8-bit store
        //     1 // 8-bit load
        //     2 // 8-bit signed load extended to 64-bits or floating point load
        //     3 // 8-bit signed load extended to 32-bits or floating point store
        //     4 // 16-bit store
        //     5 // 16-bit load
        //     6 // 16-bit signed load extended to 64-bits
        //     7 // 16-bit signed load extended to 32-bits
        //     8 // 32-bit store
        //     9 // 32-bit load
        //    10 // 32-bit signed load load extended to 64-bits (not supported for fp)
        //    12 // 64-bit store
        //    13 // 64-bit load
        //    14 // prefetch
        match op {
            0 | 4 | 8 | 12 => {
                // Store registry value into memory.
                let reg_val = if self.is_fp_register() {
                    emulate.cpu.d(register_index).to_le_bytes()
                } else if register_index == 31 {
                    0_u64.to_le_bytes()
                } else {
                    emulate.cpu.x(register_index).to_le_bytes()
                };
                match size {
                    LoadStoreRegisterByteCount::One => {
                        emulate.write_memory(address, &reg_val[..1]).await?
                    }
                    LoadStoreRegisterByteCount::Two => {
                        emulate.write_memory(address, &reg_val[..2]).await?
                    }
                    LoadStoreRegisterByteCount::Four => {
                        emulate.write_memory(address, &reg_val[..4]).await?
                    }
                    LoadStoreRegisterByteCount::Eight => {
                        emulate.write_memory(address, &reg_val[..8]).await?
                    }
                    LoadStoreRegisterByteCount::FloatingPoint => {
                        emulate.write_memory(address, &reg_val[..]).await?
                    }
                }
            }
            2 if self.is_fp_register() => {
                let reg_val = emulate.cpu.q(register_index).to_le_bytes();
                emulate.write_memory(address, &reg_val[..]).await?;
            }
            3 if self.is_fp_register() => {
                let mut new_val = [0_u8; 16];
                emulate.read_memory(address, &mut new_val[..]).await?;
                let new_val = u128::from_ne_bytes(new_val);
                emulate.cpu.update_q(register_index, new_val);
            }
            1 | 2 | 3 | 5 | 6 | 7 | 9 | 10 | 13 => {
                // Load register with new value from memory.
                let mut buf = [0_u8; 8];
                let new_val = match size {
                    LoadStoreRegisterByteCount::One => {
                        emulate.read_memory(address, &mut buf[..1]).await?;
                        u8::from_ne_bytes(buf[..1].try_into().unwrap()) as u64
                    }
                    LoadStoreRegisterByteCount::Two => {
                        emulate.read_memory(address, &mut buf[..2]).await?;
                        u16::from_ne_bytes(buf[..2].try_into().unwrap()) as u64
                    }
                    LoadStoreRegisterByteCount::Four => {
                        emulate.read_memory(address, &mut buf[..4]).await?;
                        u32::from_ne_bytes(buf[..4].try_into().unwrap()) as u64
                    }
                    LoadStoreRegisterByteCount::Eight => {
                        emulate.read_memory(address, &mut buf[..]).await?;
                        u64::from_ne_bytes(buf[..].try_into().unwrap())
                    }
                    _ => return Err(Box::new(Error::UnsupportedInstruction(self.0))),
                };
                let new_val = if op == 2 && (new_val & 0x80) != 0 {
                    // Sign extend a byte into eight bytes
                    0xffffffff_ffffff00 | new_val
                } else if op == 3 && (new_val & 0x80) != 0 {
                    // Sign extend a byte into four bytes
                    0xffffff00 | new_val
                } else if op == 6 && (new_val & 0x8000) != 0 {
                    // Sign extend two bytes into eight btes
                    0xffffffff_ffff0000 | new_val
                } else if op == 7 && (new_val & 0x8000) != 0 {
                    // Sign extend two bytes into four bytes
                    0xffff0000 | new_val
                } else if op == 10 && (new_val & 0x80000000) != 0 {
                    // Sign extend four bytes into eight bytes
                    0xffffffff_00000000 | new_val
                } else {
                    new_val
                };
                if self.is_fp_register() {
                    emulate.cpu.update_d(register_index, new_val);
                } else if register_index != 31 {
                    emulate.cpu.update_x(register_index, new_val);
                }
            }
            _ => return Err(Box::new(Error::UnsupportedInstruction(self.0))),
        }
        if matches!(
            op_group,
            Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex
                | Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex
        ) {
            // Update the index register with the end value.
            if self.rn() < 31 {
                emulate.cpu.update_x(self.rn(), end_address);
            } else {
                emulate.cpu.update_sp(end_address);
            }
        }
        Ok(())
    }
}

pub struct LoadStoreRegisterPair(pub u32);
impl LoadStoreRegisterPair {
    fn size<E>(
        &self,
        op_group: &Aarch64DecodeLoadStoreGroup,
    ) -> Result<(LoadStoreRegisterByteCount, bool), Error<E>> {
        let mut sign_extend = false;
        let size = match (self.0 >> 30) as u8 {
            0 => LoadStoreRegisterByteCount::Four,
            1 if self.is_fp_register() => LoadStoreRegisterByteCount::Eight,
            // Support for LDPSW; STGP not currently supported.
            1 if self.is_load()
                && matches!(
                    op_group,
                    Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex
                        | Aarch64DecodeLoadStoreGroup::RegisterPairOffset
                        | Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex
                ) =>
            {
                sign_extend = true;
                LoadStoreRegisterByteCount::Four
            }
            2 if self.is_fp_register() => LoadStoreRegisterByteCount::FloatingPoint,
            2 => LoadStoreRegisterByteCount::Eight,
            _ => {
                return Err(Error::UnsupportedInstruction(self.0));
            }
        };
        Ok((size, sign_extend))
    }

    // register name: V
    fn is_fp_register(&self) -> bool {
        (self.0 & 0x04000000) != 0
    }

    fn is_load(&self) -> bool {
        (self.0 & 0x00400000) != 0
    }

    fn imm(&self) -> i64 {
        let unsigned = ((self.0 >> 15) & 0x7f) as u16;
        if (unsigned & 0x40) != 0 {
            (0xff80 | unsigned) as i16 as i64
        } else {
            unsigned as i64
        }
    }

    fn rt2(&self) -> u8 {
        ((self.0 >> 10) & 0x1f) as u8
    }

    fn rn(&self) -> u8 {
        ((self.0 >> 5) & 0x1f) as u8
    }

    fn rt(&self) -> u8 {
        (self.0 & 0x1f) as u8
    }

    pub async fn emulate<T: Cpu>(
        &self,
        emulate: &mut EmulatorOperations<T>,
    ) -> Result<(), Box<Error<T::Error>>> {
        let op_group = decode_load_store_group(self.0)?;
        // p649
        let (size, sign_extend) = self.size(&op_group)?;
        let size_bytes: usize = match size {
            LoadStoreRegisterByteCount::Four => 4,
            LoadStoreRegisterByteCount::Eight => 8,
            LoadStoreRegisterByteCount::FloatingPoint => 16,
            _ => {
                return Err(Box::new(Error::UnsupportedInstruction(self.0)));
            }
        };
        let start_address = if self.rn() != 31 {
            emulate.cpu.x(self.rn())
        } else {
            emulate.cpu.sp()
        };
        let end_address = (start_address as i64 + self.imm() * (size_bytes as i64)) as u64;
        let address = if matches!(op_group, Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex) {
            start_address
        } else {
            end_address
        };
        if self.is_load() {
            // N.B. We could read both values at once, but many of our virtual
            //      devices' MMIO handlers expect specific sizes, so break the
            //      operation into two separate accesses.
            let mut value = [0_u8; 16];
            let get_val = |val: &[u8]| match size {
                LoadStoreRegisterByteCount::Four => {
                    let val = u32::from_le_bytes(val.try_into().unwrap());
                    if sign_extend && (val & 0x80000000) != 0 {
                        val as u128 | 0xffffffff_00000000
                    } else {
                        val as u128
                    }
                }
                LoadStoreRegisterByteCount::Eight => {
                    u64::from_le_bytes(val.try_into().unwrap()) as u128
                }
                LoadStoreRegisterByteCount::FloatingPoint => {
                    u128::from_le_bytes(val.try_into().unwrap())
                }
                _ => unreachable!(),
            };
            let val = &mut value[..size_bytes];
            let val1 = {
                emulate.read_memory(address, val).await?;
                get_val(val)
            };
            let val2 = {
                emulate
                    .read_memory(address + (size_bytes as u64), val)
                    .await?;
                get_val(val)
            };
            if self.is_fp_register() {
                emulate.cpu.update_q(self.rt(), val1);
            } else if self.rt() != 31 {
                emulate.cpu.update_x(self.rt(), val1 as u64);
            }
            if self.is_fp_register() {
                emulate.cpu.update_q(self.rt2(), val2);
            } else if self.rt2() != 31 {
                emulate.cpu.update_x(self.rt2(), val2 as u64);
            }
        } else {
            let mut value = [0_u8; 16];
            let val1 = if self.is_fp_register() {
                emulate.cpu.q(self.rt())
            } else if self.rt() != 31 {
                emulate.cpu.x(self.rt()) as u128
            } else {
                0
            };
            let val2 = if self.is_fp_register() {
                emulate.cpu.q(self.rt2())
            } else if self.rt2() != 31 {
                emulate.cpu.x(self.rt2()) as u128
            } else {
                0
            };
            let get_val = |val: u128| match size {
                LoadStoreRegisterByteCount::Four => {
                    ((val & 0xffffffff) as u32).to_le_bytes().to_vec()
                }
                LoadStoreRegisterByteCount::Eight => {
                    ((val & 0xffffffff_ffffffff) as u64).to_le_bytes().to_vec()
                }
                LoadStoreRegisterByteCount::FloatingPoint => val.to_le_bytes().to_vec(),
                _ => unreachable!(),
            };
            // N.B. We could write both values at once, but many of our virtual
            //      devices' MMIO handlers expect specific sizes, so break the
            //      operation into two separate accesses.
            value[..size_bytes].copy_from_slice(get_val(val1).as_slice());
            emulate.write_memory(address, &value[..size_bytes]).await?;
            value[..size_bytes].copy_from_slice(get_val(val2).as_slice());
            emulate.write_memory(address, &value[..size_bytes]).await?;
        }
        if matches!(
            op_group,
            Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex
                | Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex
        ) {
            // Update the index register with the end value.
            if self.rn() < 31 {
                emulate.cpu.update_x(self.rn(), end_address);
            } else {
                emulate.cpu.update_sp(end_address);
            }
        }
        Ok(())
    }
}

pub struct LoadStoreAtomic(pub u32);
impl LoadStoreAtomic {
    fn size(&self) -> u64 {
        1 << (self.0 >> 30)
    }

    fn rs(&self) -> u8 {
        ((self.0 >> 16) & 0x1f) as u8
    }

    fn op3_opc(&self) -> u8 {
        ((self.0 >> 12) & 0xf) as u8
    }

    fn rn(&self) -> u8 {
        ((self.0 >> 5) & 0x1f) as u8
    }

    fn rt(&self) -> u8 {
        (self.0 & 0x1f) as u8
    }

    pub async fn emulate<T: Cpu>(
        &self,
        emulate: &mut EmulatorOperations<T>,
    ) -> Result<(), Box<Error<T::Error>>> {
        let rn = self.rn();
        let address = if rn < 31 {
            emulate.cpu.x(self.rn())
        } else {
            emulate.cpu.sp()
        };
        let size = self.size();
        let size_mask = if size < 8 {
            (1 << (8 * self.size())) - 1
        } else {
            0xffffffff_ffffffff
        };
        let rt = self.rt();
        let rs = self.rs();
        let update_val = if rs != 31 {
            emulate.cpu.x(rs) & size_mask
        } else {
            0
        };
        loop {
            let mut value_buf = [0_u8; 8];
            emulate.read_memory(address, &mut value_buf).await?;
            let value = u64::from_le_bytes(value_buf);
            let sized_value = value & size_mask;
            let new_value = match self.op3_opc() {
                0 => sized_value.wrapping_add(update_val),
                1 => sized_value & !update_val,
                2 => sized_value ^ update_val,
                3 => sized_value | update_val,
                4 => match size {
                    1 => (sized_value as i8).max(update_val as i8) as u64,
                    2 => (sized_value as i16).max(update_val as i16) as u64,
                    4 => (sized_value as i32).max(update_val as i32) as u64,
                    8 => (sized_value as i64).max(update_val as i64) as u64,
                    _ => unreachable!(),
                },
                5 => match size {
                    1 => (sized_value as i8).min(update_val as i8) as u64,
                    2 => (sized_value as i16).min(update_val as i16) as u64,
                    4 => (sized_value as i32).min(update_val as i32) as u64,
                    8 => (sized_value as i64).min(update_val as i64) as u64,
                    _ => unreachable!(),
                },
                6 => sized_value.max(update_val),
                7 => sized_value.min(update_val),
                8 => update_val,
                12 => value,
                _ => {
                    return Err(Box::new(Error::UnsupportedLoadStoreInstruction(
                        Aarch64DecodeLoadStoreGroup::Atomic,
                        self.0,
                    )))
                }
            };
            let new_value = value & !size_mask | new_value & size_mask;
            if emulate
                .compare_and_write_memory(address, &value_buf, &new_value.to_le_bytes()[..])
                .await?
            {
                if rt != 31 {
                    emulate.cpu.update_x(rt, sized_value);
                }
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_groups() {
        let check_groups = [
            (0, Aarch64DecodeGroup::Reserved),
            (0x80000000, Aarch64DecodeGroup::ScalableMatrixExtension),
            (0x04000000, Aarch64DecodeGroup::ScalableVectorExtension),
            (0x84000000, Aarch64DecodeGroup::ScalableVectorExtension),
            (0x10000000, Aarch64DecodeGroup::ImmediateData),
            (0x12000000, Aarch64DecodeGroup::ImmediateData),
            (0x90000000, Aarch64DecodeGroup::ImmediateData),
            (0x92000000, Aarch64DecodeGroup::ImmediateData),
            (0x14000000, Aarch64DecodeGroup::BranchesExceptionsAndSystem),
            (0x16000000, Aarch64DecodeGroup::BranchesExceptionsAndSystem),
            (0x94000000, Aarch64DecodeGroup::BranchesExceptionsAndSystem),
            (0x96000000, Aarch64DecodeGroup::BranchesExceptionsAndSystem),
            (0x0a000000, Aarch64DecodeGroup::RegisterData),
            (0x1a000000, Aarch64DecodeGroup::RegisterData),
            (0x8a000000, Aarch64DecodeGroup::RegisterData),
            (0x9a000000, Aarch64DecodeGroup::RegisterData),
            (0x0e000000, Aarch64DecodeGroup::FpAndAdvancedData),
            (0x1e000000, Aarch64DecodeGroup::FpAndAdvancedData),
            (0x8e000000, Aarch64DecodeGroup::FpAndAdvancedData),
            (0x9e000000, Aarch64DecodeGroup::FpAndAdvancedData),
        ];
        for (opcode, expected) in check_groups {
            match decode_group::<()>(opcode) {
                Ok(result) => {
                    if result != expected {
                        panic!(
                            "{:08x} generated {:?}, expected {:?}",
                            opcode, result, expected
                        )
                    }
                }
                Err(err) => panic!(
                    "{:08x} failed with {}, expected {:?}",
                    opcode, err, expected
                ),
            }
        }

        let check_invalid_groups = [0x02000000, 0x82000000, 0x06000000, 0x86000000];
        for opcode in check_invalid_groups {
            assert!(
                decode_group::<()>(opcode).is_err(),
                "{:08x} expected to be invalid",
                opcode,
            );
        }
    }

    #[test]
    fn verify_load_store_groups() {
        let check_groups = [
            (0x08200000, Aarch64DecodeLoadStoreGroup::CompareAndSwapPair),
            (0x083f0000, Aarch64DecodeLoadStoreGroup::CompareAndSwapPair),
            (0x48200000, Aarch64DecodeLoadStoreGroup::CompareAndSwapPair),
            (0x483f0000, Aarch64DecodeLoadStoreGroup::CompareAndSwapPair),
            (0x08000000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x08010000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x08020000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x08040000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x08080000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x08100000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x48000000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x481f0000, Aarch64DecodeLoadStoreGroup::ExclusiveRegister),
            (0x88200000, Aarch64DecodeLoadStoreGroup::ExclusivePair),
            (0x883f0000, Aarch64DecodeLoadStoreGroup::ExclusivePair),
            (0xc8200000, Aarch64DecodeLoadStoreGroup::ExclusivePair),
            (0xc83f0000, Aarch64DecodeLoadStoreGroup::ExclusivePair),
            (0x08800000, Aarch64DecodeLoadStoreGroup::Ordered),
            (0x08810000, Aarch64DecodeLoadStoreGroup::Ordered),
            (0x08820000, Aarch64DecodeLoadStoreGroup::Ordered),
            (0x08840000, Aarch64DecodeLoadStoreGroup::Ordered),
            (0x08880000, Aarch64DecodeLoadStoreGroup::Ordered),
            (0x08900000, Aarch64DecodeLoadStoreGroup::Ordered),
            (0x08a00000, Aarch64DecodeLoadStoreGroup::CompareAndSwap),
            (0x08a10000, Aarch64DecodeLoadStoreGroup::CompareAndSwap),
            (0x08a20000, Aarch64DecodeLoadStoreGroup::CompareAndSwap),
            (0x08a40000, Aarch64DecodeLoadStoreGroup::CompareAndSwap),
            (0x08a80000, Aarch64DecodeLoadStoreGroup::CompareAndSwap),
            (0x08b00000, Aarch64DecodeLoadStoreGroup::CompareAndSwap),
            (
                0x0c000000,
                Aarch64DecodeLoadStoreGroup::AdvancedSimdMultiStruct,
            ),
            (
                0x0c800000,
                Aarch64DecodeLoadStoreGroup::AdvancedSimdMultiStructPostIndex,
            ),
            (
                0x4c800000,
                Aarch64DecodeLoadStoreGroup::AdvancedSimdMultiStructPostIndex,
            ),
            (0x0d000000, Aarch64DecodeLoadStoreGroup::AdvancedSimd),
            (0x4d000000, Aarch64DecodeLoadStoreGroup::AdvancedSimd),
            (0x0d200000, Aarch64DecodeLoadStoreGroup::AdvancedSimd),
            (0x4d200000, Aarch64DecodeLoadStoreGroup::AdvancedSimd),
            (
                0x0d800000,
                Aarch64DecodeLoadStoreGroup::AdvancedSimdPostIndex,
            ),
            (
                0x4d800000,
                Aarch64DecodeLoadStoreGroup::AdvancedSimdPostIndex,
            ),
            (0xd9200000, Aarch64DecodeLoadStoreGroup::MemoryTags),
            (0xd9210000, Aarch64DecodeLoadStoreGroup::MemoryTags),
            (0xd9220000, Aarch64DecodeLoadStoreGroup::MemoryTags),
            (0xd9240000, Aarch64DecodeLoadStoreGroup::MemoryTags),
            (0xd9280000, Aarch64DecodeLoadStoreGroup::MemoryTags),
            (0xd9300000, Aarch64DecodeLoadStoreGroup::MemoryTags),
            (0x19000000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x19010000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x19020000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x19040000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x19080000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x19100000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x59000000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x59010000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x59020000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x59040000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x59080000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x59100000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x99000000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x99010000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x99020000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x99040000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x99080000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x99100000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0xd9000000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0xd9010000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0xd9020000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0xd9040000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0xd9080000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0xd9100000, Aarch64DecodeLoadStoreGroup::UnscaledImmediate),
            (0x19000400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x19010400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x19020400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x19040400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x19080400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x19100400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x59000400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x59010400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x59020400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x59040400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x59080400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x59100400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x99000400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x99010400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x99020400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x99040400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x99080400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x99100400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0xd9000400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0xd9010400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0xd9020400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0xd9040400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0xd9080400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0xd9100400, Aarch64DecodeLoadStoreGroup::MemoryCopyAndSet),
            (0x18000000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0x18800000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0x58000000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0x58800000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0x98000000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0x98800000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0xd8000000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0xd8800000, Aarch64DecodeLoadStoreGroup::RegisterLiteral),
            (0x28000000, Aarch64DecodeLoadStoreGroup::NoAllocatePair),
            (0x6c000000, Aarch64DecodeLoadStoreGroup::NoAllocatePair),
            (0xac000000, Aarch64DecodeLoadStoreGroup::NoAllocatePair),
            (0xe8000000, Aarch64DecodeLoadStoreGroup::NoAllocatePair),
            (
                0x28800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex,
            ),
            (
                0x6c800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex,
            ),
            (
                0xac800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex,
            ),
            (
                0xe8800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPostIndex,
            ),
            (0x29000000, Aarch64DecodeLoadStoreGroup::RegisterPairOffset),
            (0x6d000000, Aarch64DecodeLoadStoreGroup::RegisterPairOffset),
            (0xad000000, Aarch64DecodeLoadStoreGroup::RegisterPairOffset),
            (0xe9000000, Aarch64DecodeLoadStoreGroup::RegisterPairOffset),
            (
                0x29800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex,
            ),
            (
                0x6d800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex,
            ),
            (
                0xad800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex,
            ),
            (
                0xe9800000,
                Aarch64DecodeLoadStoreGroup::RegisterPairPreIndex,
            ),
            (
                0x39000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0x3d800000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0x7d000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0x79800000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0xb9000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0xbd800000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0xfd000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0xf9800000,
                Aarch64DecodeLoadStoreGroup::RegisterUnsignedImmediate,
            ),
            (
                0x38000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x3c010000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x3c020000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x38040000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x38080000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x3c100000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x3c810000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x38820000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x38840000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x3c880000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x3c900000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x78000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x78010000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x7c020000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x7c040000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x78080000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x78100000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x7c810000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x7c820000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x7c840000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x78880000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0x78900000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xbc000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xbc010000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xb8020000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xb8040000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xbc080000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xbc100000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xb8810000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xb8820000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xbc840000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xbc880000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xb8900000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xf8000000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xfc010000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xfc020000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xf8040000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xf8080000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xfc100000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xfc810000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xf8820000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xf8840000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xfc880000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (
                0xfc900000,
                Aarch64DecodeLoadStoreGroup::RegisterUnscaledImmediate,
            ),
            (0x38200000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x38210000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x3c220000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x3c240000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x38280000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x38300000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x3ca10000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x3ca20000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x38a40000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x38a80000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x3cb00000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x7c200000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x78210000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x78220000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x7c240000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x7c280000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x78300000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x78a10000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x7ca20000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x7ca40000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x78a80000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0x78b00000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xbc200000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xbc210000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xb8220000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xb8240000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xbc280000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xbc300000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xb8a10000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xb8a20000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xbca40000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xbca80000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xb8b00000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xf8200000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xfc210000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xfc220000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xf8240000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xf8280000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xfc300000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xfca10000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xf8a20000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xf8a40000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xfca80000, Aarch64DecodeLoadStoreGroup::Atomic),
            (0xfcb00000, Aarch64DecodeLoadStoreGroup::Atomic),
            (
                0x38000400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x38010400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x3c020400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x3c040400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x38080400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x38100400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x3c810400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x3c820400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x38840400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x38880400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x3c100400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x7c000400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x78010400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x78020400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x7c040400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x7c080400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x78100400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x78810400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x7c820400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x7c840400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x78880400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x78900400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xbc000400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xbc010400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xb8020400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xb8040400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xbc080400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xbc100400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xb8810400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xb8820400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xbc840400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xbc880400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xb8900400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xf8000400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xfc010400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xfc020400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xf8040400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xf8080400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xfc100400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xfc810400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xf8820400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xf8840400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xfc880400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0xfc900400,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePostIndex,
            ),
            (
                0x38000800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x38010800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x3c020800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x3c040800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x38080800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x38100800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x3c810800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x3c820800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x38840800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x38880800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x3c900800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x7c000800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x78010800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x78020800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x7c040800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x7c080800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x78100800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x78810800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x7c820800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x7c840800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x78880800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0x78900800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xbc000800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xbc010800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xb8020800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xb8040800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xbc080800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xbc100800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xb8810800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xb8820800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xbc840800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xbc880800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xb8900800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xf8000800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xfc010800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xfc020800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xf8040800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xf8080800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xfc100800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xfc810800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xf8820800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xf8840800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xfc880800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (
                0xfc900800,
                Aarch64DecodeLoadStoreGroup::RegisterUnprivileged,
            ),
            (0x38200800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x38210800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x3c220800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x3c240800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x38280800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x38300800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x3ca10800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x3ca20800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x38a40800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x38a80800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x3cb00800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x7c200800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x78210800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x78220800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x7c240800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x7c280800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x78300800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x78a10800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x7ca20800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x7ca40800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x78a80800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0x78b00800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xbc200800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xbc210800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xb8220800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xb8240800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xbc280800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xbc300800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xb8a10800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xb8a20800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xbca40800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xbca80800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xb8b00800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xf8200800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xfc210800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xfc220800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xf8240800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xf8280800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xfc300800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xfca10800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xf8a20800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xf8a40800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xfca80800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (0xfcb00800, Aarch64DecodeLoadStoreGroup::RegisterOffset),
            (
                0x38000c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x38010c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x3c020c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x3c040c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x38080c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x38100c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x3c810c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x3c820c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x38840c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x38880c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x3c900c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x7c000c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x78010c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x78020c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x7c040c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x7c080c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x78100c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x78810c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x7c820c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x7c840c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x78880c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0x78900c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xbc000c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xbc010c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xb8020c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xb8040c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xbc080c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xbc100c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xb8810c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xb8820c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xbc840c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xbc880c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xb8900c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xf8000c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xfc010c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xfc020c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xf8040c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xf8080c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xfc100c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xfc810c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xf8820c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xf8840c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xfc880c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (
                0xfc900c00,
                Aarch64DecodeLoadStoreGroup::RegisterImmediatePreIndex,
            ),
            (0x38200400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38210400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3c220400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3c240400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38280400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38300400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3ca10400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3ca20400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38a40400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38a80400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3cb00400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7c200400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78210400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78220400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7c240400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7c280400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78300400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78a10400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7ca20400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7ca40400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78a80400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78b00400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc200400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc210400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8220400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8240400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc280400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc300400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8a10400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8a20400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbca40400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbca80400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8b00400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8200400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfc210400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfc220400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8240400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8280400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfc300400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfca10400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8a20400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8a40400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfca80400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfcb00400, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38200c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38210c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3c220c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3c240c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38280c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38300c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3ca10c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3ca20c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38a40c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x38a80c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x3cb00c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7c200c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78210c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78220c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7c240c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7c280c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78300c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78a10c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7ca20c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x7ca40c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78a80c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0x78b00c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc200c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc210c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8220c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8240c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc280c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbc300c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8a10c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8a20c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbca40c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xbca80c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xb8b00c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8200c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfc210c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfc220c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8240c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8280c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfc300c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfca10c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8a20c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xf8a40c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfca80c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
            (0xfcb00c00, Aarch64DecodeLoadStoreGroup::RegisterPac),
        ];
        for (opcode, expected) in check_groups {
            match decode_group::<()>(opcode) {
                Ok(result) => {
                    let expected = Aarch64DecodeGroup::LoadStore(expected);
                    if result != expected {
                        panic!(
                            "{:08x} generated {:?}, expected {:?}",
                            opcode, result, expected
                        )
                    }
                }
                Err(err) => panic!(
                    "{:08x} failed with {}, expected {:?}",
                    opcode, err, expected
                ),
            }
        }
        let check_invalid_groups = [
            0x0c010000, 0x0c200000, 0x0ca00000, 0x0d010000, 0x0d020000, 0x0d040000, 0x0d080000,
            0x0d100000, 0x4d1f0000,
        ];
        for opcode in check_invalid_groups {
            assert!(
                decode_group::<()>(opcode).is_err(),
                "{:08x} expected to be invalid",
                opcode
            );
        }
    }
}
