// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use aarch64emu::AccessCpuState;
use aarch64emu::Cpu;
use aarch64emu::Emulator;
use aarch64emu::InterceptState;
use pal_async::async_test;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

#[derive(Debug)]
pub enum TestCpuError {
    BadAddress,
}

#[derive(Clone, Debug, Default)]
pub struct CpuState {
    x: [Arc<AtomicU64>; 31],
    q: Arc<Mutex<[u128; 32]>>,
    pc: Arc<AtomicU64>,
    sp: Arc<AtomicU64>,
    cpsr: Arc<AtomicU64>,
    instruction: Arc<AtomicU32>,
}

impl CpuState {
    fn x(&mut self, index: u8) -> u64 {
        self.x[index as usize].load(Ordering::Relaxed)
    }
    fn update_x(&mut self, index: u8, data: u64) {
        self.x[index as usize].store(data, Ordering::Relaxed)
    }
    fn q(&self, index: u8) -> u128 {
        self.q.lock()[index as usize]
    }
    fn update_q(&mut self, index: u8, data: u128) {
        self.q.lock()[index as usize] = data
    }
    fn sp(&mut self) -> u64 {
        self.sp.load(Ordering::Relaxed)
    }
    fn update_sp(&mut self, data: u64) {
        self.sp.store(data, Ordering::Relaxed)
    }
    fn pc(&mut self) -> u64 {
        self.pc.load(Ordering::Relaxed)
    }
    fn update_pc(&mut self, data: u64) {
        self.pc.store(data, Ordering::Relaxed)
    }
    fn cpsr(&mut self) -> aarch64defs::Cpsr64 {
        self.cpsr.load(Ordering::Relaxed).into()
    }
    fn instruction(&self) -> u32 {
        self.instruction.load(Ordering::Relaxed)
    }
    fn update_instruction(&mut self, instruction: u32) {
        self.instruction.store(instruction, Ordering::Relaxed)
    }
}

#[derive(Debug, Default)]
pub struct SingleCellCpu {
    pub valid_gva: u64,
    pub mem_val: Arc<futures::lock::Mutex<u128>>,
    cpu_state: CpuState,
}

impl SingleCellCpu {
    pub fn new(cpu_state: CpuState) -> Self {
        Self {
            cpu_state,
            ..Default::default()
        }
    }
}

impl Cpu for SingleCellCpu {
    type Error = TestCpuError;

    async fn read_instruction(&mut self, gva: u64, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if gva == self.cpu_state.pc() && bytes.len() == 4 {
            bytes.copy_from_slice(self.cpu_state.instruction().to_le_bytes().as_slice());
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn read_memory(&mut self, gva: u64, bytes: &mut [u8]) -> Result<(), Self::Error> {
        let mem_val = self.mem_val.lock().await;
        let end_mem = self.valid_gva as usize + size_of_val(&*mem_val);
        println!(
            "read gva = {:016x}-{:016x} from range = {:016x}-{:016x}",
            gva,
            gva as usize + bytes.len() - 1,
            self.valid_gva,
            end_mem - 1
        );
        if gva >= self.valid_gva && end_mem >= (gva as usize + bytes.len()) {
            let begin = (gva - self.valid_gva) as usize;
            let end = begin + bytes.len();
            bytes.copy_from_slice(&(*mem_val).to_le_bytes().as_ref()[begin..end]);
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn read_physical_memory(
        &mut self,
        _gpa: u64,
        _bytes: &mut [u8],
    ) -> Result<(), Self::Error> {
        panic!("Not expected to be used during tests");
    }

    async fn write_memory(&mut self, gva: u64, bytes: &[u8]) -> Result<(), Self::Error> {
        let mut mem_val = self.mem_val.lock().await;
        let end_mem = self.valid_gva as usize + size_of_val(&*mem_val);
        println!(
            "write gva = {:016x}-{:016x} from range = {:016x}-{:016x}",
            gva,
            gva as usize + bytes.len() - 1,
            self.valid_gva,
            end_mem - 1
        );
        if gva >= self.valid_gva && end_mem >= (gva as usize + bytes.len()) {
            let begin = (gva - self.valid_gva) as usize;
            let end = begin + bytes.len();
            let mut val = (*mem_val).to_le_bytes();
            val.as_mut()[begin..end].copy_from_slice(bytes);
            *mem_val = u128::from_le_bytes(val);
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn write_physical_memory(&mut self, _gpa: u64, _bytes: &[u8]) -> Result<(), Self::Error> {
        panic!("Not expected to be used during tests");
    }

    async fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        success: &mut bool,
    ) -> Result<(), Self::Error> {
        let mut mem_val = self.mem_val.lock().await;
        if gva == self.valid_gva {
            if &(*mem_val).to_le_bytes().as_ref()[..current.len()] == current {
                let mut val = (*mem_val).to_le_bytes();
                val.as_mut()[..new.len()].copy_from_slice(new);
                *mem_val = u128::from_le_bytes(val);
                *success = true;
            } else {
                *success = false;
            }
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }
}

impl AccessCpuState for SingleCellCpu {
    fn commit(&mut self) {}
    fn x(&mut self, index: u8) -> u64 {
        self.cpu_state.x(index)
    }
    fn update_x(&mut self, index: u8, data: u64) {
        self.cpu_state.update_x(index, data)
    }
    fn q(&self, index: u8) -> u128 {
        self.cpu_state.q(index)
    }
    fn update_q(&mut self, index: u8, data: u128) {
        self.cpu_state.update_q(index, data)
    }
    fn d(&self, index: u8) -> u64 {
        (self.q(index) & 0xffffffff_ffffffff) as u64
    }
    fn update_d(&mut self, index: u8, data: u64) {
        self.update_q(index, data as u128);
    }
    fn h(&self, index: u8) -> u32 {
        (self.d(index) & 0xffffffff) as u32
    }
    fn update_h(&mut self, index: u8, data: u32) {
        self.update_q(index, data as u128);
    }
    fn s(&self, index: u8) -> u16 {
        (self.h(index) & 0xffff) as u16
    }
    fn update_s(&mut self, index: u8, data: u16) {
        self.update_q(index, data as u128);
    }
    fn b(&self, index: u8) -> u8 {
        (self.s(index) & 0xff) as u8
    }
    fn update_b(&mut self, index: u8, data: u8) {
        self.update_q(index, data as u128);
    }
    fn sp(&mut self) -> u64 {
        self.cpu_state.sp()
    }
    fn update_sp(&mut self, data: u64) {
        self.cpu_state.update_sp(data)
    }
    fn fp(&mut self) -> u64 {
        self.cpu_state.x(29)
    }
    fn update_fp(&mut self, data: u64) {
        self.cpu_state.update_x(29, data)
    }
    fn lr(&mut self) -> u64 {
        self.cpu_state.x(30)
    }
    fn update_lr(&mut self, data: u64) {
        self.cpu_state.update_x(30, data)
    }
    fn pc(&mut self) -> u64 {
        self.cpu_state.pc()
    }
    fn update_pc(&mut self, data: u64) {
        self.cpu_state.update_pc(data)
    }
    fn cpsr(&mut self) -> aarch64defs::Cpsr64 {
        self.cpu_state.cpsr()
    }
}

#[derive(Clone, Copy)]
enum OffsetInRegister {
    Bytes64(u8),
    Step64(u8),
    UnsignedBytes32(u8),
    UnsignedStep32(u8),
    SignedBytes32(u8),
    SignedStep32(u8),
}

// Decode register offset into register(rm) with option and shift bits
const fn from_register_offset(offset: &OffsetInRegister) -> (u8, u32) {
    match offset {
        OffsetInRegister::Bytes64(rm) => (*rm, 7 << 13),
        OffsetInRegister::Step64(rm) => (*rm, 7 << 13 | 1 << 12),
        OffsetInRegister::UnsignedBytes32(rm) => (*rm, 2 << 13),
        OffsetInRegister::UnsignedStep32(rm) => (*rm, 2 << 13 | 1 << 12),
        OffsetInRegister::SignedBytes32(rm) => (*rm, 6 << 13),
        OffsetInRegister::SignedStep32(rm) => (*rm, 6 << 13 | 1 << 12),
    }
}

const fn ldrb_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38400c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrb_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38400400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrb_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x39400000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}

const fn ldrb_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, mut option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    // Step encoding not supported for byte access -- ignore it.
    option_and_s &= 0xffffefff;
    0x38600800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}

const fn ldrh_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78400c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrh_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78400400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrh_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x79400000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrh_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    0x78600800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}

const fn ldr32_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8400c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldr32_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8400400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldr32_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb9400000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldr32_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8600800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}
const fn ldr32_literal(pc_offset_words: u32, rt: u8) -> u32 {
    assert!(rt < 32);
    assert!(pc_offset_words & 0xfff80000 == 0);
    0x18000000 | pc_offset_words << 5 | (rt as u32)
}

const fn ldr64_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xf8400c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldr64_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xf8400400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldr64_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xf9400000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldr64_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    0xf8600800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}
const fn ldr64_literal(pc_offset_words: u32, rt: u8) -> u32 {
    assert!(rt < 32);
    assert!(pc_offset_words & 0xfff80000 == 0);
    0x58000000 | pc_offset_words << 5 | (rt as u32)
}

const fn ldrsb32_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38c00c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsb32_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38c00400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsb32_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x39c00000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsb32_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, mut option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    // Step encoding not supported for byte access -- ignore it.
    option_and_s &= 0xffffefff;
    0x38e00800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}

const fn ldrsb64_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38800c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsb64_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38800400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsb64_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x39800000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsb64_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, mut option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    // Step encoding not supported for byte access -- ignore it.
    option_and_s &= 0xffffefff;
    0x38a00800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}

const fn ldrsh32_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78c00c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsh32_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78c00400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsh32_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x79c00000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsh32_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    0x78e00800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}

const fn ldrsh64_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78800c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsh64_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78800400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsh64_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x79800000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsh64_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    0x78a00800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}

const fn ldrsw_pre(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8800c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsw_post(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8800400 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsw_u(imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb9800000 | (imm12 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsw_reg(offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8a00800 | (rm as u32) << 16 | option_and_s | (rn as u32) << 5 | (rt as u32)
}
const fn ldrsw_literal(pc_offset_words: u32, rt: u8) -> u32 {
    assert!(rt < 32);
    assert!(pc_offset_words & 0xfff80000 == 0);
    0x98000000 | pc_offset_words << 5 | (rt as u32)
}

const fn ldurb(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38400000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldurh(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78400000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldur32(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8400000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldur64(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xf8400000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}

const fn ldursb32(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38c00000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldursb64(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x38800000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldursh32(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78c00000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldursh64(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0x78800000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}
const fn ldursw(imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    0xb8800000 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
}

const fn ldrx_fp_pre(bytes: u8, imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    let (byte_size, use_fp_size) = if bytes == 16 {
        (0, 1_u32 << 23)
    } else if bytes == 8 {
        (3, 0)
    } else if bytes == 4 {
        (2, 0)
    } else if bytes == 2 {
        (1, 0)
    } else {
        assert!(bytes == 1);
        (0, 0)
    };
    0x3c400c00
        | (byte_size as u32) << 30
        | use_fp_size
        | (imm9 as u32) << 12
        | (rn as u32) << 5
        | (rt as u32)
}
const fn ldrx_fp_post(bytes: u8, imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    let (byte_size, use_fp_size) = if bytes == 16 {
        (0, 1_u32 << 23)
    } else if bytes == 8 {
        (3, 0)
    } else if bytes == 4 {
        (2, 0)
    } else if bytes == 2 {
        (1, 0)
    } else {
        assert!(bytes == 1);
        (0, 0)
    };
    0x3c400400
        | (byte_size as u32) << 30
        | use_fp_size
        | (imm9 as u32) << 12
        | (rn as u32) << 5
        | (rt as u32)
}
const fn ldrx_fp_u(bytes: u8, imm12: u16, rn: u8, rt: u8) -> u32 {
    if (imm12 & 0xf000) != 0 {
        panic!("Invalid imm12 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    let (byte_size, use_fp_size) = if bytes == 16 {
        (0, 1_u32 << 23)
    } else if bytes == 8 {
        (3, 0)
    } else if bytes == 4 {
        (2, 0)
    } else if bytes == 2 {
        (1, 0)
    } else {
        assert!(bytes == 1);
        (0, 0)
    };
    0x3d400000
        | (byte_size as u32) << 30
        | use_fp_size
        | (imm12 as u32) << 10
        | (rn as u32) << 5
        | (rt as u32)
}
const fn ldrx_fp_reg(bytes: u8, offset: OffsetInRegister, rn: u8, rt: u8) -> u32 {
    let (rm, mut option_and_s) = from_register_offset(&offset);
    assert!(rm < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let (byte_size, use_fp_size) = if bytes == 16 {
        (0, 1_u32 << 23)
    } else if bytes == 8 {
        (3, 0)
    } else if bytes == 4 {
        (2, 0)
    } else if bytes == 2 {
        (1, 0)
    } else {
        assert!(bytes == 1);
        // Step encoding not supported for byte access -- ignore it.
        option_and_s &= 0xffffefff;
        (0, 0)
    };
    0x3c600800
        | (byte_size as u32) << 30
        | use_fp_size
        | (rm as u32) << 16
        | option_and_s
        | (rn as u32) << 5
        | (rt as u32)
}
const fn ldrx_fp_literal(bytes: u8, pc_offset_words: u32, rt: u8) -> u32 {
    assert!(rt < 32);
    assert!(pc_offset_words & 0xfff80000 == 0);
    let byte_size = if bytes == 16 {
        2
    } else if bytes == 8 {
        1
    } else {
        assert!(bytes == 4);
        0
    };
    0x1c000000 | byte_size << 30 | pc_offset_words << 5 | (rt as u32)
}

const fn ldur_fp(bytes: u8, imm9: u16, rn: u8, rt: u8) -> u32 {
    if (imm9 & 0xfe00) != 0 {
        panic!("Invalid imm9 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    let (byte_size, use_fp_size) = if bytes == 16 {
        (0, 1_u32 << 23)
    } else if bytes == 8 {
        (3, 0)
    } else if bytes == 4 {
        (2, 0)
    } else if bytes == 2 {
        (1, 0)
    } else {
        assert!(bytes == 1);
        (0, 0)
    };
    0x3c400000
        | (byte_size << 30)
        | use_fp_size
        | (imm9 as u32) << 12
        | (rn as u32) << 5
        | (rt as u32)
}

const fn ldp32(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0x29400000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldp32_pre(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0x29c00000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldp32_post(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0x28c00000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldpsw(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0x69400000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldpsw_pre(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0x69c00000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldpsw_post(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0x68c00000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldp64(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0xa9400000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldp64_pre(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0xa9c00000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}
const fn ldp64_post(imm7: u16, rn: u8, rt: u8, rt2: u8) -> u32 {
    if (imm7 & 0xff80) != 0 {
        panic!("Invalid imm7 value");
    }
    assert!(rn < 32);
    assert!(rt < 32);
    assert!(rt2 < 32);
    0xa8c00000 | (imm7 as u32) << 15 | (rt2 as u32) << 10 | (rn as u32) << 5 | (rt as u32)
}

// Aliased as STADDx and STADDLx.
const fn ldadd(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38200000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

const fn ldclr(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38201000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

const fn ldeor(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38202000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

const fn ldset(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38203000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

// Aliased as STSMAXx and STSMAXLx
const fn ldsmax(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38204000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

// Aliased as STSMINx and STSMINLx
const fn ldsmin(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38205000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

// Aliased as STUMAXx and STUMAXLx
const fn ldumax(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38206000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

// Aliased as STUMINx and STUMINLx
const fn ldumin(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38207000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

const fn swp(size: u8, acquire: bool, release: bool, rs: u8, rn: u8, rt: u8) -> u32 {
    assert!(rs < 32);
    assert!(rn < 32);
    assert!(rt < 32);
    let acquire = if acquire { 0x00800000 } else { 0 };
    let release = if release { 0x00400000 } else { 0 };
    0x38208000
        | (size as u32) << 30
        | acquire
        | release
        | (rs as u32) << 16
        | (rn as u32) << 5
        | (rt as u32)
}

const fn ldapr(size: u8, rn: u8, rt: u8) -> u32 {
    assert!(rn < 32);
    assert!(rt < 32);
    0x38bfc000 | (size as u32) << 30 | (rn as u32) << 5 | (rt as u32)
}

//
// TODO: Add store tests
//
// const fn strb(imm9: u16, rn: u8, rt: u8) -> u32 {
//     if (imm9 & 0xfe00) != 0 {
//         panic!("Invalid imm9 value");
//     }
//     assert!(rn < 32);
//     assert!(rt < 32);
//     0x38000c00 | (imm9 as u32) << 12 | (rn as u32) << 5 | (rt as u32)
// }

#[async_test]
async fn verify_load_register_literal() {
    let variations = [
        (ldr32_literal(0xc0, 12), 4, 0xc0, 12, false, false),
        (ldr32_literal(0x7ff40, 3), 4, -0xc0, 3, false, false),
        (ldr32_literal(0x100, 31), 4, 0x100, 31, false, false),
        (ldr64_literal(0x1c0, 1), 8, 0x1c0, 1, false, false),
        (ldr64_literal(0x7fe40, 19), 8, -0x1c0, 19, false, false),
        (ldrsw_literal(0x100, 22), 4, 0x100, 22, false, true),
        (ldrsw_literal(0x7fff0, 11), 4, -0x10, 11, false, true),
        (ldrx_fp_literal(4, 0x500, 1), 4, 0x500, 1, true, false),
        (ldrx_fp_literal(4, 0x7ff00, 1), 4, -0x100, 1, true, false),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_10000000);
    for (op, bytes, off, rt, is_fp, signed) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        if is_fp {
            cpu_state.update_q(rt, 0xabcdefab_cdefabcd_efabcdef_abcdefab);
        } else if rt != 31 {
            cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        }
        // N.B. The PC value should be advanced by the emulator each loop.
        let pc = cpu_state.pc();
        cpu.valid_gva = pc.wrapping_add((off * 4) as i64 as u64);
        let expected = if signed && *cpu.mem_val.lock().await & 0x80000000 != 0 {
            assert!(bytes == 4);
            0xffffffff_00000000 | (*cpu.mem_val.lock().await & 0xffffffff)
        } else if bytes != 8 {
            (*cpu.mem_val.lock().await) & ((1 << (8 * bytes)) - 1)
        } else if !is_fp {
            *cpu.mem_val.lock().await & 0xffffffff_ffffffff
        } else {
            *cpu.mem_val.lock().await
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        if is_fp {
            assert_eq!(cpu_state.q(rt), expected);
        } else if rt != 31 {
            assert_eq!(cpu_state.x(rt), expected as u64);
        }
        assert_eq!(cpu_state.pc(), pc + 4);
    }
}

#[async_test]
async fn verify_load_gp_register_immediate_pre_and_post_index_simple() {
    let variations = [
        (ldrb_pre(0xc0, 8, 0), 1, 8, 0xc0_i32, true, 0),
        (ldrb_pre(0x140, 9, 2), 1, 9, -0xc0, true, 2),
        (ldrb_pre(0x80, 31, 3), 1, 31, 0x80, true, 3),
        (ldrb_pre(0x80, 3, 31), 1, 3, 0x80, true, 31),
        (ldrh_pre(0xc0, 10, 3), 2, 10, 0xc0, true, 3),
        (ldrh_pre(0x1f0, 11, 4), 2, 11, -0x10, true, 4),
        (ldrh_pre(0x1f0, 31, 7), 2, 31, -0x10, true, 7),
        (ldrh_pre(0x1f0, 7, 31), 2, 7, -0x10, true, 31),
        (ldr32_pre(0xc0, 12, 5), 4, 12, 0xc0, true, 5),
        (ldr32_pre(0x1c0, 13, 7), 4, 13, -0x40, true, 7),
        (ldr32_pre(0x50, 31, 1), 4, 31, 0x50, true, 1),
        (ldr32_pre(0x50, 1, 31), 4, 1, 0x50, true, 31),
        (ldr64_pre(0x30, 15, 10), 8, 15, 0x30, true, 10),
        (ldr64_pre(0x100, 17, 22), 8, 17, -0x100, true, 22),
        (ldr64_pre(0x10, 31, 20), 8, 31, 0x10, true, 20),
        (ldr64_pre(0x10, 20, 31), 8, 20, 0x10, true, 31),
        (ldrb_post(0xc0, 8, 0), 1, 8, 0xc0_i32, false, 0),
        (ldrb_post(0x140, 9, 2), 1, 9, -0xc0, false, 2),
        (ldrb_post(0x140, 31, 30), 1, 31, -0xc0, false, 30),
        (ldrb_post(0x140, 30, 31), 1, 30, -0xc0, false, 31),
        (ldrh_post(0xc0, 10, 3), 2, 10, 0xc0, false, 3),
        (ldrh_post(0x1f0, 11, 4), 2, 11, -0x10, false, 4),
        (ldrh_post(0x180, 31, 11), 2, 31, -0x80, false, 11),
        (ldrh_post(0x180, 11, 31), 2, 11, -0x80, false, 31),
        (ldr32_post(0xc0, 12, 5), 4, 12, 0xc0, false, 5),
        (ldr32_post(0x1c0, 13, 7), 4, 13, -0x40, false, 7),
        (ldr32_post(0x20, 31, 7), 4, 31, 0x20, false, 7),
        (ldr32_post(0x20, 7, 31), 4, 7, 0x20, false, 31),
        (ldr64_post(0x30, 15, 10), 8, 15, 0x30, false, 10),
        (ldr64_post(0x100, 17, 22), 8, 17, -0x100, false, 22),
        (ldr64_post(0x100, 31, 2), 8, 31, -0x100, false, 2),
        (ldr64_post(0x100, 2, 31), 8, 2, -0x100, false, 31),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, pre, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        if rt != 31 {
            cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        }
        if rn < 31 {
            cpu_state.update_x(rn, 0x7fffffff_10000000);
        } else {
            cpu_state.update_sp(0x7fffffff_10000000);
        }
        cpu.valid_gva = if pre {
            0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64)
        } else {
            0x7fffffff_10000000_u64
        };
        let expected = if bytes != 8 {
            (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1)
        } else {
            *cpu.mem_val.lock().await as u64
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        if rt != 31 {
            assert_eq!(cpu_state.x(rt), expected);
        }
        assert_eq!(
            if rn < 31 {
                cpu_state.x(rn)
            } else {
                cpu_state.sp()
            },
            0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64)
        );
    }
}

#[async_test]
async fn verify_load_gp_register_immediate_unsigned_simple() {
    let variations = [
        (ldrb_u(0xc0, 0, 3), 1, 0, 0xc0_u64, 3),
        (ldrb_u(0xf00, 1, 0), 1, 1, 0xf00, 0),
        (ldrh_u(0xc0, 7, 15), 2, 7, 0xc0 * 2, 15),
        (ldrh_u(0xff0, 5, 2), 2, 5, 0xff0 * 2, 2),
        (ldr32_u(0xc0, 9, 27), 4, 9, 0xc0 * 4, 27),
        (ldr32_u(0xffc, 30, 0), 4, 30, 0xffc * 4, 0),
        (ldr64_u(0x30, 15, 10), 8, 15, 0x30 * 8, 10),
        (ldr64_u(0x800, 17, 22), 8, 17, 0x800 * 8, 22),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off);
        let expected = if bytes != 8 {
            (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1)
        } else {
            *cpu.mem_val.lock().await as u64
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(cpu_state.x(rt), expected);
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000);
    }
}

#[async_test]
async fn verify_load_gp_register_unscaled_immediate() {
    let variations = [
        (ldurb(0xc0, 8, 0), 1, 8, 0xc0_i32, 0),
        (ldurb(0x140, 9, 2), 1, 9, -0xc0, 2),
        (ldurh(0xc0, 8, 0), 2, 8, 0xc0_i32, 0),
        (ldurh(0x140, 9, 2), 2, 9, -0xc0, 2),
        (ldur32(0xc0, 8, 0), 4, 8, 0xc0_i32, 0),
        (ldur32(0x140, 9, 2), 4, 9, -0xc0, 2),
        (ldur64(0xc0, 8, 0), 8, 8, 0xc0_i32, 0),
        (ldur64(0x140, 9, 2), 8, 9, -0xc0, 2),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64);
        let expected = if bytes != 8 {
            (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1)
        } else {
            *cpu.mem_val.lock().await as u64
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
    }
}

#[async_test]
async fn verify_load_gp_register_register_offset() {
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrb_reg(reg_offset, rn, rt),
            1,
            0x11223344_55667766_55443322_11001122,
            0x22,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsb32_reg(reg_offset, rn, rt),
            1,
            0x11223344_55667766_55443322_11001122,
            0x22,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsb32_reg(reg_offset, rn, rt),
            1,
            0x11223344_55667766_55443322_11001182,
            0xffffff82,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsb64_reg(reg_offset, rn, rt),
            1,
            0x11223344_55667766_55443322_11001122,
            0x22,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsb64_reg(reg_offset, rn, rt),
            1,
            0x11223344_55667766_55443322_11001182,
            0xffffffff_ffffff82,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrh_reg(reg_offset, rn, rt),
            2,
            0x11223344_55667766_55443322_11001122,
            0x1122,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsh32_reg(reg_offset, rn, rt),
            2,
            0x11223344_55667766_55443322_11001122,
            0x1122,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsh32_reg(reg_offset, rn, rt),
            2,
            0x11223344_55667766_55443322_11008182,
            0xffff8182,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsh64_reg(reg_offset, rn, rt),
            2,
            0x11223344_55667766_55443322_11001122,
            0x1122,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsh64_reg(reg_offset, rn, rt),
            2,
            0x11223344_55667766_55443322_11008182,
            0xffffffff_ffff8182,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldr32_reg(reg_offset, rn, rt),
            4,
            0x11223344_55667766_55443322_11001122,
            0x11001122,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsw_reg(reg_offset, rn, rt),
            4,
            0x11223344_55667766_55443322_11001122,
            0x11001122,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldrsw_reg(reg_offset, rn, rt),
            4,
            0x11223344_55667766_55443322_81001122,
            0xffffffff_81001122,
        )
    })
    .await;
    run_register_offset_variations(|reg_offset, rn, rt| {
        (
            ldr64_reg(reg_offset, rn, rt),
            8,
            0x11223344_55667766_55443322_11001122,
            0x55443322_11001122,
        )
    })
    .await;
}

async fn run_register_offset_variations<F>(generate_inst: F)
where
    F: Fn(OffsetInRegister, u8, u8) -> (u32, u64, u128, u64),
{
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for reg_variation in 0..=5 {
        fn get_next_valid_reg_index(i: u8) -> u8 {
            i % 32
        }
        let mut register_indices = [0_u8; 3];
        getrandom::getrandom(&mut register_indices).expect("rng failure");
        let rt = get_next_valid_reg_index(register_indices[0]);
        let rn = get_next_valid_reg_index(register_indices[1]);
        let rn = if rt != 31 && rn == rt {
            get_next_valid_reg_index(rn + 1)
        } else {
            rn
        };
        let mut rm = get_next_valid_reg_index(register_indices[2]);
        while rm != 31 && (rm == rt || rm == rn) {
            rm = get_next_valid_reg_index(rm + 1);
        }
        let reg_offset = match reg_variation {
            0 => OffsetInRegister::Bytes64(rm),
            1 => OffsetInRegister::Step64(rm),
            2 => OffsetInRegister::SignedBytes32(rm),
            3 => OffsetInRegister::SignedStep32(rm),
            4 => OffsetInRegister::UnsignedBytes32(rm),
            5 => OffsetInRegister::UnsignedStep32(rm),
            _ => unreachable!(),
        };
        let (op, bytes, initial_mem_value, expected_rt_value) = generate_inst(reg_offset, rn, rt);
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        if rt != 31 {
            cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        }
        if rn < 31 {
            cpu_state.update_x(rn, 0x7fffffff_10000000);
        } else {
            cpu_state.update_sp(0x7fffffff_10000000);
        }
        for mem_variations in 0..2 {
            let mut cpu = SingleCellCpu::new(cpu_state.clone());
            *cpu.mem_val.lock().await = initial_mem_value;
            let (rm_val, off) = if rm != 31 {
                match reg_offset {
                    OffsetInRegister::Bytes64(_) => {
                        if mem_variations == 0 {
                            (0x00001000, 0x1000)
                        } else {
                            (0x1_80000000, 0x1_80000000)
                        }
                    }
                    OffsetInRegister::Step64(_) => {
                        if mem_variations == 0 {
                            (0x00001000, 0x1000 * bytes)
                        } else {
                            (0x1_00000000, 0x1_00000000 * bytes)
                        }
                    }
                    OffsetInRegister::SignedBytes32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000)
                        } else {
                            (0x1_fffff000, -0x1000_i64 as u64)
                        }
                    }
                    OffsetInRegister::SignedStep32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000 * bytes)
                        } else {
                            (0x1_ffffffff, -(bytes as i32) as i64 as u64)
                        }
                    }
                    OffsetInRegister::UnsignedBytes32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000)
                        } else {
                            (0x1_fffff000, 0xfffff000)
                        }
                    }
                    OffsetInRegister::UnsignedStep32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000 * bytes)
                        } else {
                            (0x1_fffff000, 0xfffff000 * bytes)
                        }
                    }
                }
            } else {
                (0, 0)
            };
            if rm != 31 {
                cpu_state.update_x(rm, rm_val);
            }
            cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off);
            let mut emulator = Emulator::new(cpu, &intercept_state);
            assert!(emulator.run().await.is_ok());
            if rt != 31 {
                assert_eq!(
                    cpu_state.x(rt),
                    expected_rt_value,
                    "{:08x} <-> {:08x}",
                    cpu_state.x(rt),
                    expected_rt_value
                );
            }
            assert_eq!(
                if rn < 31 {
                    cpu_state.x(rn)
                } else {
                    cpu_state.sp()
                },
                0x7fffffff_10000000_u64
            );
            if rm != 31 {
                assert_eq!(cpu_state.x(rm), rm_val);
            }
        }
    }
}

#[async_test]
async fn verify_load_gp_register_immediate_pre_and_post_index_sign_extend() {
    let variations = [
        (ldrsb32_pre(0xc0, 8, 0), false, 1, 8, 0xc0_i32, true, 0),
        (ldrsb32_pre(0x140, 9, 2), false, 1, 9, -0xc0, true, 2),
        (ldrsb64_pre(0x30, 15, 10), true, 1, 15, 0x30, true, 10),
        (ldrsb64_pre(0x100, 17, 22), true, 1, 17, -0x100, true, 22),
        (ldrsh32_pre(0xc0, 8, 0), false, 2, 8, 0xc0_i32, true, 0),
        (ldrsh32_pre(0x140, 9, 2), false, 2, 9, -0xc0, true, 2),
        (ldrsh64_pre(0x30, 15, 10), true, 2, 15, 0x30, true, 10),
        (ldrsh64_pre(0x100, 17, 22), true, 2, 17, -0x100, true, 22),
        (ldrsw_pre(0x30, 15, 10), true, 4, 15, 0x30, true, 10),
        (ldrsw_pre(0x100, 17, 22), true, 4, 17, -0x100, true, 22),
        (ldrsb32_post(0xc0, 8, 0), false, 1, 8, 0xc0_i32, false, 0),
        (ldrsb32_post(0x140, 9, 2), false, 1, 9, -0xc0, false, 2),
        (ldrsb64_post(0x30, 15, 10), true, 1, 15, 0x30, false, 10),
        (ldrsb64_post(0x100, 17, 22), true, 1, 17, -0x100, false, 22),
        (ldrsh32_post(0xc0, 8, 0), false, 2, 8, 0xc0_i32, false, 0),
        (ldrsh32_post(0x140, 9, 2), false, 2, 9, -0xc0, false, 2),
        (ldrsh64_post(0x30, 15, 10), true, 2, 15, 0x30, false, 10),
        (ldrsh64_post(0x100, 17, 22), true, 2, 17, -0x100, false, 22),
        (ldrsw_post(0x30, 15, 10), true, 4, 15, 0x30, false, 10),
        (ldrsw_post(0x100, 17, 22), true, 4, 17, -0x100, false, 22),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, is_64bit, bytes, rn, off, pre, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        cpu.valid_gva = if pre {
            0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64)
        } else {
            0x7fffffff_10000000_u64
        };
        // Check unsigned value
        *cpu.mem_val.lock().await = 0x11223344_55667766_55443322_11001122;
        let expected = (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1);
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(
            cpu_state.x(rn),
            0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64)
        );

        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        cpu.valid_gva = if pre {
            0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64)
        } else {
            0x7fffffff_10000000_u64
        };
        // Check signed value
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        *cpu.mem_val.lock().await |= 1 << (8 * bytes - 1);
        let expected = (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1)
            | if is_64bit {
                !((1 << (8 * bytes)) - 1)
            } else {
                0xffffffff & !((1 << (8 * bytes)) - 1)
            };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(
            cpu_state.x(rn),
            0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64)
        );
    }
}

#[async_test]
async fn verify_load_gp_register_immediate_unsigned_sign_extend() {
    let variations = [
        (ldrsb32_u(0xc0, 8, 0), false, 1, 8, 0xc0_u32, 0),
        (ldrsb32_u(0x840, 9, 2), false, 1, 9, 0x840, 2),
        (ldrsb64_u(0x30, 15, 10), true, 1, 15, 0x30, 10),
        (ldrsb64_u(0x800, 17, 22), true, 1, 17, 0x800, 22),
        (ldrsh32_u(0xc0, 8, 0), false, 2, 8, 0xc0 * 2, 0),
        (ldrsh32_u(0x840, 9, 2), false, 2, 9, 0x840 * 2, 2),
        (ldrsh64_u(0x30, 15, 10), true, 2, 15, 0x30 * 2, 10),
        (ldrsh64_u(0x800, 17, 22), true, 2, 17, 0x800 * 2, 22),
        (ldrsw_u(0x30, 15, 10), true, 4, 15, 0x30 * 4, 10),
        (ldrsw_u(0x800, 17, 22), true, 4, 17, 0x800 * 4, 22),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, is_64bit, bytes, rn, off, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off as u64);
        // Check unsigned value
        *cpu.mem_val.lock().await = 0x11223344_55667766_55443322_11001122;
        let expected = (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1);
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);

        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off as u64);
        // Check signed value
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        *cpu.mem_val.lock().await |= 1 << (8 * bytes - 1);
        let expected = (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1)
            | if is_64bit {
                !((1 << (8 * bytes)) - 1)
            } else {
                0xffffffff & !((1 << (8 * bytes)) - 1)
            };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
    }
}

#[async_test]
async fn verify_load_gp_register_unscaled_immediate_sign_extend() {
    let variations = [
        (ldursb32(0xc0, 8, 0), false, 1, 8, 0xc0_i32, 0),
        (ldursb32(0x140, 7, 1), false, 1, 7, -0xc0, 1),
        (ldursb64(0x90, 9, 11), true, 1, 9, 0x90, 11),
        (ldursb64(0x130, 8, 15), true, 1, 8, -0xd0, 15),
        (ldursh32(0x80, 10, 11), false, 2, 10, 0x80, 11),
        (ldursh32(0x190, 25, 26), false, 2, 25, -0x70, 26),
        (ldursh64(0xa0, 9, 5), true, 2, 9, 0xa0, 5),
        (ldursh64(0x100, 15, 3), true, 2, 15, -0x100, 3),
        (ldursw(0xc0, 0, 3), true, 4, 0, 0xc0, 3),
        (ldursw(0x140, 15, 19), true, 4, 15, -0xc0, 19),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, is_64bit, bytes, rn, off, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64);
        // Check unsigned value
        *cpu.mem_val.lock().await = 0x11223344_55667766_55443322_11001122;
        let expected = (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1);
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);

        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64);
        // Check signed value
        cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        *cpu.mem_val.lock().await |= 1 << (8 * bytes - 1);
        let expected = (*cpu.mem_val.lock().await as u64) & ((1 << (8 * bytes)) - 1)
            | if is_64bit {
                !((1 << (8 * bytes)) - 1)
            } else {
                0xffffffff & !((1 << (8 * bytes)) - 1)
            };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.x(rt),
            expected,
            "{:08x} <-> {:08x}",
            cpu_state.x(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
    }
}

#[async_test]
async fn verify_atomic_add() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldadd(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0xffffffff_010101ee);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_03030303_02020321;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let addend = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    old_val & !update_mask | old_val.wrapping_add(addend) & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_clear() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldclr(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0xffffffff_010101ee);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_aaaaaaaa_55555555;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let clear_mask = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    old_val & !update_mask | old_val & !clear_mask & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_xor() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldeor(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0xffffffff_010101ee);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_aaaaaaaa_55555555;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let xor_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    old_val & !update_mask | (old_val ^ xor_val) & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_or() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldset(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0xffffffff_010101ee);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_aaaaaaaa_55555555;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let new_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    old_val | new_val & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_signed_max() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldsmax(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0x88448844_88448888);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_77777777_77777777;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let new_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    let sign_bit = 1 << (8 * bytes - 1);
                    let max_val = if (old_val & sign_bit) != (new_val & sign_bit) {
                        if (new_val & sign_bit) != 0 {
                            old_val
                        } else {
                            new_val
                        }
                    } else {
                        (old_val & update_mask).max(new_val & update_mask)
                    };
                    old_val & !update_mask | max_val & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_signed_min() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldsmin(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0x88448844_88448888);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_77777777_77777777;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let new_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    let sign_bit = 1 << (8 * bytes - 1);
                    let min_val = if (old_val & sign_bit) != (new_val & sign_bit) {
                        if (new_val & sign_bit) != 0 {
                            new_val
                        } else {
                            old_val
                        }
                    } else {
                        (old_val & update_mask).min(new_val & update_mask)
                    };
                    old_val & !update_mask | min_val & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_unsigned_max() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldumax(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0x88448844_88448888);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_77777777_77777777;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let new_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    if (old_val & update_mask).max(new_val & update_mask) == (old_val & update_mask)
                    {
                        old_val
                    } else {
                        old_val & !update_mask | new_val & update_mask
                    }
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_unsigned_min() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = ldumin(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0x88448844_88448888);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_77777777_77777777;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let new_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    if (old_val & update_mask).min(new_val & update_mask) == (old_val & update_mask)
                    {
                        old_val
                    } else {
                        old_val & !update_mask | new_val & update_mask
                    }
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_swap() {
    let variations = [(0, 1, 2), (31, 1, 2), (0, 31, 2), (0, 1, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (acquire, release) in [(true, true), (true, false), (false, true), (false, false)] {
            for (rs, rn, rt) in variations {
                let op = swp(size, acquire, release, rs, rn, rt);
                println!("op = {:08x}", op);
                cpu_state.update_instruction(op);
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                if rt != 31 {
                    cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
                }
                if rn < 31 {
                    cpu_state.update_x(rn, 0x7fffffff_10000000);
                } else {
                    cpu_state.update_sp(0x7fffffff_10000000);
                }
                if rs < 31 {
                    cpu_state.update_x(rs, 0x88448844_88448888);
                }
                cpu.valid_gva = 0x7fffffff_10000000;
                *cpu.mem_val.lock().await = 0xffffffff_ffffffff_77777777_77777777;
                let bytes = 1 << size;
                let update_mask = if bytes < 8 {
                    (1 << (8 * bytes)) - 1
                } else {
                    0xffffffff_ffffffff
                };
                let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
                let new_val = if rs < 31 { cpu_state.x(rs) } else { 0 };
                let mem_val = cpu.mem_val.clone();
                let expected_update = {
                    let old_val = *mem_val.lock().await as u64;
                    old_val & !update_mask | new_val & update_mask
                };
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                if rt != 31 {
                    assert_eq!(
                        cpu_state.x(rt),
                        expected_reg,
                        "{:08x} <-> {:08x}",
                        cpu_state.x(rt),
                        expected_reg
                    );
                }
                let new_val = *mem_val.lock().await as u64;
                assert_eq!(
                    new_val, expected_update,
                    "{:08x} <-> {:08x}",
                    new_val, expected_update
                );
                if rn < 31 {
                    assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
                } else {
                    assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
                }
            }
        }
    }
}

#[async_test]
async fn verify_atomic_load_acquire() {
    let variations = [(1, 2), (31, 1), (0, 31)];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for size in 0_u8..4 {
        for (rn, rt) in variations {
            let op = ldapr(size, rn, rt);
            println!("op = {:08x}", op);
            cpu_state.update_instruction(op);
            let mut cpu = SingleCellCpu::new(cpu_state.clone());
            if rt != 31 {
                cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
            }
            if rn < 31 {
                cpu_state.update_x(rn, 0x7fffffff_10000000);
            } else {
                cpu_state.update_sp(0x7fffffff_10000000);
            }
            cpu.valid_gva = 0x7fffffff_10000000;
            *cpu.mem_val.lock().await = 0xffffffff_ffffffff_77777777_77777777;
            let bytes = 1 << size;
            let update_mask = if bytes < 8 {
                (1 << (8 * bytes)) - 1
            } else {
                0xffffffff_ffffffff
            };
            let expected_reg = (*cpu.mem_val.lock().await as u64) & update_mask;
            let mem_val = cpu.mem_val.clone();
            let original_val = *mem_val.lock().await as u64;
            let mut emulator = Emulator::new(cpu, &intercept_state);
            assert!(emulator.run().await.is_ok());
            if rt != 31 {
                assert_eq!(
                    cpu_state.x(rt),
                    expected_reg,
                    "{:08x} <-> {:08x}",
                    cpu_state.x(rt),
                    expected_reg
                );
            }
            let cur_val = *mem_val.lock().await as u64;
            assert_eq!(
                original_val, cur_val,
                "{:08x} <-> {:08x}",
                original_val, cur_val,
            );
            if rn < 31 {
                assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000_u64);
            } else {
                assert_eq!(cpu_state.sp(), 0x7fffffff_10000000_u64);
            }
        }
    }
}

#[async_test]
async fn verify_load_fp_register_immediate_pre_and_post_index_simple() {
    let variations = [
        (ldrx_fp_pre(1, 0xc0, 8, 0), 1, 8, 0xc0_i32, true, 0),
        (ldrx_fp_pre(1, 0x140, 9, 2), 1, 9, -0xc0, true, 2),
        (ldrx_fp_pre(2, 0xc0, 10, 3), 2, 10, 0xc0, true, 3),
        (ldrx_fp_pre(2, 0x1f0, 11, 4), 2, 11, -0x10, true, 4),
        (ldrx_fp_pre(4, 0xc0, 12, 5), 4, 12, 0xc0, true, 5),
        (ldrx_fp_pre(4, 0x1c0, 13, 7), 4, 13, -0x40, true, 7),
        (ldrx_fp_pre(8, 0x30, 15, 10), 8, 15, 0x30, true, 10),
        (ldrx_fp_pre(8, 0x100, 17, 22), 8, 17, -0x100, true, 22),
        (ldrx_fp_pre(16, 0x30, 15, 31), 16, 15, 0x30, true, 31),
        (ldrx_fp_pre(16, 0x100, 17, 22), 16, 17, -0x100, true, 22),
        (ldrx_fp_post(1, 0xc0, 8, 0), 1, 8, 0xc0, false, 0),
        (ldrx_fp_post(1, 0x140, 9, 2), 1, 9, -0xc0, false, 2),
        (ldrx_fp_post(2, 0xc0, 10, 3), 2, 10, 0xc0, false, 3),
        (ldrx_fp_post(2, 0x1f0, 11, 4), 2, 11, -0x10, false, 4),
        (ldrx_fp_post(4, 0xc0, 12, 5), 4, 12, 0xc0, false, 5),
        (ldrx_fp_post(4, 0x1c0, 13, 7), 4, 13, -0x40, false, 7),
        (ldrx_fp_post(8, 0x30, 15, 10), 8, 15, 0x30, false, 10),
        (ldrx_fp_post(8, 0x100, 17, 22), 8, 17, -0x100, false, 22),
        (ldrx_fp_post(16, 0x30, 15, 31), 16, 15, 0x30, false, 31),
        (ldrx_fp_post(16, 0x100, 17, 22), 16, 17, -0x100, false, 22),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, pre, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        cpu_state.update_q(rt, 0xabcdefab_cdefabcd_efabcdef_abcdefab);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = if pre {
            0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64)
        } else {
            0x7fffffff_10000000_u64
        };
        let expected = if bytes != 16 {
            *cpu.mem_val.lock().await & ((1 << (8 * bytes)) - 1)
        } else {
            *cpu.mem_val.lock().await
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.q(rt),
            expected,
            "{:016x} <-> {:016x}",
            cpu_state.q(rt),
            expected
        );
        assert_eq!(
            cpu_state.x(rn),
            0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64)
        );
    }
}

#[async_test]
async fn verify_load_fp_register_immediate_unsigned_simple() {
    let variations = [
        (ldrx_fp_u(1, 0xc0, 0, 3), 1, 0, 0xc0_u64, 3),
        (ldrx_fp_u(1, 0xf00, 1, 0), 1, 1, 0xf00, 0),
        (ldrx_fp_u(2, 0xc0, 7, 15), 2, 7, 0xc0 * 2, 15),
        (ldrx_fp_u(2, 0xff0, 5, 2), 2, 5, 0xff0 * 2, 2),
        (ldrx_fp_u(4, 0xc0, 9, 27), 4, 9, 0xc0 * 4, 27),
        (ldrx_fp_u(4, 0xffc, 30, 0), 4, 30, 0xffc * 4, 0),
        (ldrx_fp_u(8, 0x30, 15, 10), 8, 15, 0x30 * 8, 10),
        (ldrx_fp_u(8, 0x800, 17, 22), 8, 17, 0x800 * 8, 22),
        (ldrx_fp_u(16, 0x30, 15, 10), 16, 15, 0x30 * 16, 10),
        (ldrx_fp_u(16, 0x800, 17, 31), 16, 17, 0x800 * 16, 31),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        cpu_state.update_q(rt, 0xabcdefab_cdefabcd_efabcdef_abcdefab);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off);
        let expected = if bytes != 16 {
            *cpu.mem_val.lock().await & ((1 << (8 * bytes)) - 1)
        } else {
            *cpu.mem_val.lock().await
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.q(rt),
            expected,
            "{:016x} <-> {:016x}",
            cpu_state.q(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000);
    }
}

#[async_test]
async fn verify_load_fp_register_unscaled_immediate() {
    let variations = [
        (ldur_fp(1, 0xc0, 0, 3), 1, 0, 0xc0_i32, 3),
        (ldur_fp(1, 0x140, 1, 0), 1, 1, -0xc0, 0),
        (ldur_fp(2, 0xc0, 7, 15), 2, 7, 0xc0, 15),
        (ldur_fp(2, 0x100, 5, 2), 2, 5, -0x100, 2),
        (ldur_fp(4, 0xc0, 9, 27), 4, 9, 0xc0, 27),
        (ldur_fp(4, 0x120, 30, 0), 4, 30, -0xe0, 0),
        (ldur_fp(8, 0x30, 15, 10), 8, 15, 0x30, 10),
        (ldur_fp(8, 0x140, 17, 22), 8, 17, -0xc0, 22),
        (ldur_fp(16, 0x30, 15, 10), 16, 15, 0x30, 10),
        (ldur_fp(16, 0x150, 17, 31), 16, 17, -0xb0, 31),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, rt) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        cpu_state.update_q(rt, 0xabcdefab_cdefabcd_efabcdef_abcdefab);
        cpu_state.update_x(rn, 0x7fffffff_10000000);
        cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off as u64);
        let expected = if bytes != 16 {
            *cpu.mem_val.lock().await & ((1 << (8 * bytes)) - 1)
        } else {
            *cpu.mem_val.lock().await
        };
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        assert_eq!(
            cpu_state.q(rt),
            expected,
            "{:016x} <-> {:016x}",
            cpu_state.q(rt),
            expected
        );
        assert_eq!(cpu_state.x(rn), 0x7fffffff_10000000);
    }
}

#[async_test]
async fn verify_load_fp_register_register_offset() {
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for bytes in [1_u64, 2, 4, 8, 16] {
        for reg_variation in 0..=5 {
            fn get_next_valid_reg_index(i: u8, allow_sp: bool) -> u8 {
                let valid_index = i % 32;
                let valid_index = if valid_index == 31 && !allow_sp {
                    valid_index + 1
                } else {
                    valid_index
                };
                valid_index % 32
            }
            let mut register_indices = [0_u8; 3];
            getrandom::getrandom(&mut register_indices).expect("rng failure");
            // fp available registers are q0-q31
            let rt = register_indices[0] % 32;
            let rn = get_next_valid_reg_index(register_indices[1], true);
            let mut rm = get_next_valid_reg_index(register_indices[2], false);
            if rm == rn {
                rm = get_next_valid_reg_index(rm + 1, false);
            }
            let reg_offset = match reg_variation {
                0 => OffsetInRegister::Bytes64(rm),
                1 => OffsetInRegister::Step64(rm),
                2 => OffsetInRegister::SignedBytes32(rm),
                3 => OffsetInRegister::SignedStep32(rm),
                4 => OffsetInRegister::UnsignedBytes32(rm),
                5 => OffsetInRegister::UnsignedStep32(rm),
                _ => unreachable!(),
            };
            let op = ldrx_fp_reg(bytes as u8, reg_offset, rn, rt);
            println!("op = {:08x}", op);
            cpu_state.update_instruction(op);
            cpu_state.update_q(rt, 0xabcdefab_cdefabcd_efabcdef_abcdefab);
            if rn < 31 {
                cpu_state.update_x(rn, 0x7fffffff_10000000);
            } else {
                cpu_state.update_sp(0x7fffffff_10000000);
            }
            for mem_variations in 0..2 {
                let mut cpu = SingleCellCpu::new(cpu_state.clone());
                *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
                let expected_rt_value = if bytes != 16 {
                    *cpu.mem_val.lock().await & ((1 << (8 * bytes)) - 1)
                } else {
                    *cpu.mem_val.lock().await
                };
                let (rm_val, off) = match reg_offset {
                    OffsetInRegister::Bytes64(_) => {
                        if mem_variations == 0 {
                            (0x00001000, 0x1000)
                        } else {
                            (0x1_80000000, 0x1_80000000)
                        }
                    }
                    OffsetInRegister::Step64(_) => {
                        if mem_variations == 0 {
                            (0x00001000, 0x1000 * bytes)
                        } else {
                            (0x1_00000000, 0x1_00000000 * bytes)
                        }
                    }
                    OffsetInRegister::SignedBytes32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000)
                        } else {
                            (0x1_fffff000, -0x1000_i64 as u64)
                        }
                    }
                    OffsetInRegister::SignedStep32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000 * bytes)
                        } else {
                            (0x1_ffffffff, -(bytes as i32) as i64 as u64)
                        }
                    }
                    OffsetInRegister::UnsignedBytes32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000)
                        } else {
                            (0x1_fffff000, 0xfffff000)
                        }
                    }
                    OffsetInRegister::UnsignedStep32(_) => {
                        if mem_variations == 0 {
                            (0x1_00001000, 0x1000_u64 * bytes)
                        } else {
                            (0x1_fffff000, 0xfffff000 * bytes)
                        }
                    }
                };
                cpu_state.update_x(rm, rm_val);
                cpu.valid_gva = 0x7fffffff_10000000_u64.wrapping_add(off);
                let mut emulator = Emulator::new(cpu, &intercept_state);
                assert!(emulator.run().await.is_ok());
                assert_eq!(
                    cpu_state.q(rt),
                    expected_rt_value,
                    "{:016x} <-> {:016x}",
                    cpu_state.q(rt),
                    expected_rt_value
                );
                assert_eq!(
                    if rn < 31 {
                        cpu_state.x(rn)
                    } else {
                        cpu_state.sp()
                    },
                    0x7fffffff_10000000_u64
                );
                assert_eq!(cpu_state.x(rm), rm_val);
            }
        }
    }
}

#[async_test]
async fn verify_load_gp_register_pair() {
    enum AdjustIndex {
        Unchanged,
        PostIncrement,
        PreIncrement,
    }
    let variations = [
        (
            ldp64(0x20, 8, 0, 1),
            8,
            8,
            0x100_i32,
            AdjustIndex::Unchanged,
            0,
            1,
        ),
        (
            ldp64(0x30, 31, 31, 5),
            8,
            31,
            0x180_i32,
            AdjustIndex::Unchanged,
            31,
            5,
        ),
        (
            ldp64(0x40, 8, 2, 3),
            8,
            8,
            -0x200_i32,
            AdjustIndex::Unchanged,
            2,
            3,
        ),
        (
            ldp64_pre(0x18, 20, 10, 13),
            8,
            20,
            0xc0_i32,
            AdjustIndex::PreIncrement,
            10,
            13,
        ),
        (
            ldp64_pre(0x48, 20, 10, 13),
            8,
            20,
            -0x1c0_i32,
            AdjustIndex::PreIncrement,
            10,
            13,
        ),
        (
            ldp64_post(0, 28, 8, 3),
            8,
            28,
            0_i32,
            AdjustIndex::PostIncrement,
            8,
            3,
        ),
        (
            ldp64_post(0x48, 20, 10, 13),
            8,
            20,
            -0x1c0_i32,
            AdjustIndex::PostIncrement,
            10,
            13,
        ),
        (
            ldp32(0x20, 8, 0, 1),
            4,
            8,
            0x80_i32,
            AdjustIndex::Unchanged,
            0,
            1,
        ),
        (
            ldp32(0x30, 31, 31, 5),
            4,
            31,
            0xc0_i32,
            AdjustIndex::Unchanged,
            31,
            5,
        ),
        (
            ldp32(0x40, 8, 2, 3),
            4,
            8,
            -0x100_i32,
            AdjustIndex::Unchanged,
            2,
            3,
        ),
        (
            ldp32_pre(0x18, 20, 10, 13),
            4,
            20,
            0x60_i32,
            AdjustIndex::PreIncrement,
            10,
            13,
        ),
        (
            ldp32_pre(0x48, 20, 10, 13),
            4,
            20,
            -0xe0_i32,
            AdjustIndex::PreIncrement,
            10,
            13,
        ),
        (
            ldp32_post(0, 28, 8, 3),
            4,
            28,
            0_i32,
            AdjustIndex::PostIncrement,
            8,
            3,
        ),
        (
            ldp32_post(0x48, 20, 10, 13),
            4,
            20,
            -0xe0_i32,
            AdjustIndex::PostIncrement,
            10,
            13,
        ),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, bytes, rn, off, adj_index, rt, rt2) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        let mut cpu = SingleCellCpu::new(cpu_state.clone());
        *cpu.mem_val.lock().await = 0x11223344_55667788_99aabbcc_ddeeff00;
        if rt != 31 {
            cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
        }
        if rt2 != 31 {
            cpu_state.update_x(rt2, 0xabcdefab_cdefabcd);
        }
        if rn < 31 {
            cpu_state.update_x(rn, 0x7fffffff_10000000);
        } else {
            cpu_state.update_sp(0x7fffffff_10000000);
        }
        cpu.valid_gva = if !matches!(adj_index, AdjustIndex::PostIncrement) {
            0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64)
        } else {
            0x7fffffff_10000000_u64
        };
        let expected_rt = (*cpu.mem_val.lock().await & ((1 << (8 * bytes)) - 1)) as u64;
        let expected_rt2 =
            ((*cpu.mem_val.lock().await >> (8 * bytes)) & ((1 << (8 * bytes)) - 1)) as u64;
        let mut emulator = Emulator::new(cpu, &intercept_state);
        assert!(emulator.run().await.is_ok());
        if rt != 31 {
            assert_eq!(cpu_state.x(rt), expected_rt);
        }
        if rt2 != 31 {
            assert_eq!(cpu_state.x(rt2), expected_rt2);
        }
        if !matches!(adj_index, AdjustIndex::Unchanged) {
            assert_eq!(
                if rn < 31 {
                    cpu_state.x(rn)
                } else {
                    cpu_state.sp()
                },
                0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64)
            );
        }
    }
}

#[async_test]
async fn verify_load_gp_register_pair_sign_extend() {
    enum AdjustIndex {
        Unchanged,
        PostIncrement,
        PreIncrement,
    }
    let variations = [
        (
            ldpsw(0x20, 8, 0, 1),
            8,
            0x80_i32,
            AdjustIndex::Unchanged,
            0,
            1,
        ),
        (
            ldpsw(0x30, 31, 31, 5),
            31,
            0xc0_i32,
            AdjustIndex::Unchanged,
            31,
            5,
        ),
        (
            ldpsw(0x40, 8, 2, 3),
            8,
            -0x100_i32,
            AdjustIndex::Unchanged,
            2,
            3,
        ),
        (
            ldpsw_pre(0x18, 20, 10, 13),
            20,
            0x60_i32,
            AdjustIndex::PreIncrement,
            10,
            13,
        ),
        (
            ldpsw_pre(0x48, 20, 10, 13),
            20,
            -0xe0_i32,
            AdjustIndex::PreIncrement,
            10,
            13,
        ),
        (
            ldpsw_post(0, 28, 8, 3),
            28,
            0_i32,
            AdjustIndex::PostIncrement,
            8,
            3,
        ),
        (
            ldpsw_post(0x48, 20, 10, 13),
            20,
            -0xe0_i32,
            AdjustIndex::PostIncrement,
            10,
            13,
        ),
    ];
    let intercept_state = InterceptState::default();
    let mut cpu_state = CpuState::default();
    cpu_state.update_pc(0x7fffffff_00000000);
    for (op, rn, off, adj_index, rt, rt2) in variations {
        println!("op = {:08x}", op);
        cpu_state.update_instruction(op);
        for val in [
            0x11111111_22222222_12345678_76543210,
            0x11111111_22222222_12345678_87654321,
            0x11111111_22222222_87654321_01234567,
            0x11111111_22222222_cba98765_87654321,
        ] {
            let mut cpu = SingleCellCpu::new(cpu_state.clone());
            *cpu.mem_val.lock().await = val;
            if rt != 31 {
                cpu_state.update_x(rt, 0xabcdefab_cdefabcd);
            }
            if rt2 != 31 {
                cpu_state.update_x(rt2, 0xabcdefab_cdefabcd);
            }
            if rn < 31 {
                cpu_state.update_x(rn, 0x7fffffff_10000000);
            } else {
                cpu_state.update_sp(0x7fffffff_10000000);
            }
            cpu.valid_gva = if !matches!(adj_index, AdjustIndex::PostIncrement) {
                0x7fffffff_10000000_u64.wrapping_add(off as i64 as u64)
            } else {
                0x7fffffff_10000000_u64
            };
            let expected_rt = *cpu.mem_val.lock().await as u32 as i32 as i64 as u64;
            let expected_rt2 = (*cpu.mem_val.lock().await >> 32) as u32 as i32 as i64 as u64;
            let mut emulator = Emulator::new(cpu, &intercept_state);
            assert!(emulator.run().await.is_ok());
            if rt != 31 {
                assert_eq!(cpu_state.x(rt), expected_rt);
            }
            if rt2 != 31 {
                assert_eq!(cpu_state.x(rt2), expected_rt2);
            }
            if !matches!(adj_index, AdjustIndex::Unchanged) {
                assert_eq!(
                    if rn < 31 {
                        cpu_state.x(rn)
                    } else {
                        cpu_state.sp()
                    },
                    0x7fffffff_10000000_u64.wrapping_add((off as i64) as u64)
                );
            }
        }
    }
}
