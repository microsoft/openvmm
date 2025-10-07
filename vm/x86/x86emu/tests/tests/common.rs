// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use futures::FutureExt;
use iced_x86::IcedError;
use iced_x86::code_asm::CodeAssembler;
use std::fmt::Debug;
use x86defs::RFlags;
use x86defs::SegmentAttributes;
use x86defs::SegmentRegister;
use x86defs::cpuid::Vendor;
use x86emu::Cpu;
use x86emu::Emulator;
use x86emu::Error;
use x86emu::RegisterIndex;
use x86emu::Segment;

#[derive(Debug, Clone, PartialEq)]
pub struct CpuState {
    /// GP registers, in the canonical order (as defined by `RAX`, etc.).
    pub gps: [u64; 16],
    /// Segment registers, in the canonical order (as defined by `ES`, etc.).
    /// Immutable for now.
    pub segs: [SegmentRegister; 6],
    /// RIP.
    pub rip: u64,
    /// RFLAGS.
    pub rflags: RFlags,
    /// CR0. Immutable.
    pub cr0: u64,
    /// EFER. Immutable.
    pub efer: u64,
}

/// The mask of flags that are changed by an arithmetic (add, sub, cmp) operation.
pub const RFLAGS_ARITH_MASK: RFlags = RFlags::new()
    .with_overflow(true)
    .with_sign(true)
    .with_zero(true)
    .with_adjust(true)
    .with_parity(true)
    .with_carry(true);

/// The mask of flags that are changed by a logical (and, or, etc) operation.
pub const RFLAGS_LOGIC_MASK: RFlags = RFlags::new()
    .with_overflow(true)
    .with_sign(true)
    .with_zero(true)
    .with_parity(true)
    .with_carry(true);

pub fn run_test(
    rflags_mask: RFlags,
    asm: impl Fn(&mut CodeAssembler) -> Result<(), IcedError>,
    set_state: impl Fn(&mut SingleCellCpu<u64>),
) -> SingleCellCpu<u64> {
    run_lockable_test(rflags_mask, LockTestBehavior::DecodeError, asm, set_state)
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum LockTestBehavior {
    /// The instruction should fail to complete if the memory cell is mutated
    /// after it has been read.
    Fail,
    /// As with `Fail`, but the `lock` prefix is implicit on the instruction
    /// under test (i.e. `xchg`).
    FailImplicitLock,
    /// The instruction should still complete if the memory cell is mutated
    /// after it has been read.
    Succeed,
    /// The instruction under test should fail to decode if the `lock` prefix is
    /// present.
    DecodeError,
}

pub fn run_lockable_test<T: TestRegister>(
    rflags_mask: RFlags,
    behavior: LockTestBehavior,
    asm: impl Fn(&mut CodeAssembler) -> Result<(), IcedError>,
    set_state: impl Fn(&mut SingleCellCpu<T>),
) -> SingleCellCpu<T> {
    let asm = &asm;
    let set_state = &set_state;

    // Run the test three times. Once without lock prefix, once with lock prefix
    // but no other changes, and once with lock prefix, mutating the value after
    // it has been read.

    // Unlocked test.
    let unlocked_cpu = run_test_core(rflags_mask, true, asm, set_state).unwrap();

    if behavior == LockTestBehavior::DecodeError {
        match *run_test_core(rflags_mask, true, |x| asm(x.lock()), set_state).unwrap_err() {
            Error::DecodeFailure => {}
            err => panic!("unexpected error: {err}"),
        }
    } else {
        // Successful locked test.
        let mut locked_cpu =
            run_test_core(rflags_mask, true, |x| asm(x.lock()), set_state).unwrap();

        // Move the rip back by the size of the lock prefix.
        let rip = locked_cpu.rip();
        locked_cpu.set_rip(rip - 1);
        assert_eq!(
            unlocked_cpu.state(),
            locked_cpu.state(),
            "lock success state should match unlocked state"
        );
        assert_eq!(
            unlocked_cpu, locked_cpu,
            "lock success cpu should match unlocked cpu"
        );

        let mut init_cpu = SingleCellCpu::new(0.into());

        set_state(&mut init_cpu);

        // Make sure that lock failure doesn't change the CPU state.
        let test_lock_failure = |lock_prefix| {
            let mut lock_failure_cpu = run_test_core(
                if behavior == LockTestBehavior::Succeed {
                    rflags_mask
                } else {
                    0.into()
                },
                behavior == LockTestBehavior::Succeed,
                |x| if lock_prefix { asm(x.lock()) } else { asm(x) },
                |cpu| {
                    set_state(cpu);
                    cpu.invert_after_read = true;
                },
            )
            .unwrap();

            lock_failure_cpu.invert_after_read = false;
            lock_failure_cpu.invert_mem_val();

            if behavior == LockTestBehavior::Succeed {
                let lock_failure_rip = lock_failure_cpu.rip();
                lock_failure_cpu.set_rip(lock_failure_rip - 1);
                assert_eq!(
                    lock_failure_cpu, unlocked_cpu,
                    "lock failure cpu should match unlocked cpu"
                );
                assert_eq!(
                    lock_failure_cpu.state(),
                    unlocked_cpu.state(),
                    "lock failure state should match unlocked state"
                );
            } else {
                assert_eq!(
                    lock_failure_cpu, init_cpu,
                    "lock failure cpu should match init cpu"
                );
                assert_eq!(
                    lock_failure_cpu.state(),
                    init_cpu.state(),
                    "lock failure state should match init state"
                );
            }
        };
        test_lock_failure(true);
        if behavior == LockTestBehavior::FailImplicitLock {
            test_lock_failure(false);
        }
    }

    unlocked_cpu
}

pub fn run_u128_test(
    rflags_mask: RFlags,
    asm: impl Fn(&mut CodeAssembler) -> Result<(), IcedError>,
    set_state: impl Fn(&mut SingleCellCpu<u128>),
) -> SingleCellCpu<u128> {
    run_lockable_test(rflags_mask, LockTestBehavior::DecodeError, asm, set_state)
}

pub fn run_wide_test(
    rflags_mask: RFlags,
    should_finish: bool,
    asm: impl Fn(&mut CodeAssembler) -> Result<(), IcedError>,
    set_state: impl Fn(&mut MultipleCellCpu),
) -> MultipleCellCpu {
    run_test_core(rflags_mask, should_finish, asm, set_state).unwrap()
}

fn run_test_core<T: TestCpu>(
    rflags_mask: RFlags,
    incr_rip: bool,
    asm: impl Fn(&mut CodeAssembler) -> Result<(), IcedError>,
    set_state: impl Fn(&mut T),
) -> Result<T, Box<Error<<T as Cpu>::Error>>> {
    let mut zero_cpu = run_one_test(0.into(), rflags_mask, incr_rip, &asm, &set_state)?;
    let mut one_cpu = run_one_test((!0).into(), rflags_mask, incr_rip, &asm, &set_state)?;

    assert_eq!(
        zero_cpu, one_cpu,
        "Behavior differed across different starting RFLAGS values."
    );
    assert_eq!(
        zero_cpu.rflags() & rflags_mask,
        one_cpu.rflags() & rflags_mask,
        "Produced RFLAGS values differed across different starting RFLAGS values."
    );

    one_cpu.set_rflags(zero_cpu.rflags());
    assert_eq!(
        zero_cpu.state(),
        one_cpu.state(),
        "Behavior differed across different starting RFLAGS values."
    );
    Ok(zero_cpu)
}

fn run_one_test<T: TestCpu>(
    mut init_rflags: RFlags,
    rflags_mask: RFlags,
    incr_rip: bool,
    asm: &impl Fn(&mut CodeAssembler) -> Result<(), IcedError>,
    set_state: &impl Fn(&mut T),
) -> Result<T, Box<Error<<T as Cpu>::Error>>> {
    // Unset trap, we want to run to completion always.
    init_rflags.set_trap(false);
    let mut cpu = T::new(init_rflags);
    set_state(&mut cpu);
    let starting_rflags = cpu.rflags();

    let mut assembler = CodeAssembler::new(64).unwrap();
    asm(&mut assembler).unwrap();
    let rip = cpu.rip();
    let emulator_input = assembler.assemble(rip).unwrap();

    Emulator::new(&mut cpu, Vendor::INTEL, &emulator_input)
        .run()
        .now_or_never()
        .unwrap()?;

    let inverse_mask = (!u64::from(rflags_mask)).into();
    assert_eq!(
        starting_rflags & inverse_mask,
        cpu.rflags() & inverse_mask,
        "Bits outside of rflags_mask were changed."
    );

    if incr_rip {
        assert_eq!(
            cpu.rip(),
            emulator_input.len() as u64,
            "RIP did not match asm length."
        );
    } else {
        assert_eq!(
            cpu.rip(),
            0,
            "RIP was incremented when it shouldn't have been."
        );
    }

    Ok(cpu)
}

#[derive(Debug)]
pub enum TestCpuError {
    BadAddress,
    BadLength,
}

#[derive(Debug)]
pub struct SingleCellCpu<T: TestRegister> {
    pub valid_gva: u64,
    pub mem_val: T,
    pub valid_io_port: u16,
    pub io_val: u32,
    pub xmm: [u128; 16],
    pub invert_after_read: bool,
    pub state: CpuState,
}

impl<T: TestRegister> SingleCellCpu<T> {
    pub fn invert_mem_val(&mut self) {
        let mut v = self.mem_val.to_le_bytes();
        for b in v.as_mut() {
            *b = !*b;
        }
        self.mem_val = T::from_le_bytes(v);
    }
}

impl<T: TestRegister> Cpu for SingleCellCpu<T> {
    type Error = TestCpuError;

    async fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        _is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        if gva == self.valid_gva {
            bytes.copy_from_slice(&self.mem_val.to_le_bytes().as_ref()[..bytes.len()]);
            if self.invert_after_read {
                self.invert_mem_val();
            }
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
        _is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        if gva == self.valid_gva {
            let mut val = self.mem_val.to_le_bytes();
            val.as_mut()[..bytes.len()].copy_from_slice(bytes);
            self.mem_val = T::from_le_bytes(val);
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn compare_and_write_memory(
        &mut self,
        gva: u64,
        current: &[u8],
        new: &[u8],
        _is_user_mode: bool,
    ) -> Result<bool, Self::Error> {
        if gva == self.valid_gva {
            if &self.mem_val.to_le_bytes().as_ref()[..current.len()] == current {
                let mut val = self.mem_val.to_le_bytes();
                val.as_mut()[..new.len()].copy_from_slice(new);
                self.mem_val = T::from_le_bytes(val);
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn read_io(&mut self, io_port: u16, data: &mut [u8]) -> Result<(), Self::Error> {
        if io_port == self.valid_io_port {
            data.copy_from_slice(&self.io_val.to_le_bytes()[..data.len()]);
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn write_io(&mut self, io_port: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        if io_port == self.valid_io_port {
            let mut val = [0; 4];
            val[..bytes.len()].copy_from_slice(bytes);
            self.io_val = u32::from_le_bytes(val);
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    fn gp(&mut self, reg: RegisterIndex) -> u64 {
        reg.apply_sizing(self.state.gps[reg.extended_index as usize])
    }

    fn gp_sign_extend(&mut self, reg: RegisterIndex) -> i64 {
        reg.apply_sizing_signed(self.state.gps[reg.extended_index as usize])
    }

    fn set_gp(&mut self, reg: RegisterIndex, v: u64) {
        let val = reg.apply_update(self.state.gps[reg.extended_index as usize], v);
        self.state.gps[reg.extended_index as usize] = val;
    }

    fn rip(&mut self) -> u64 {
        self.state.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.state.rip = v
    }

    fn segment(&mut self, reg: Segment) -> SegmentRegister {
        self.state.segs[reg as usize]
    }

    fn efer(&mut self) -> u64 {
        self.state.efer
    }

    fn cr0(&mut self) -> u64 {
        self.state.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.state.rflags
    }
    fn set_rflags(&mut self, v: RFlags) {
        self.state.rflags = v
    }

    fn xmm(&mut self, reg: usize) -> u128 {
        self.xmm[reg]
    }

    fn set_xmm(&mut self, reg: usize, value: u128) {
        self.xmm[reg] = value;
    }
}

#[derive(Debug)]
pub struct MultipleCellCpu {
    pub valid_gva: u64,
    pub mem_val: Vec<u8>,
    pub valid_io_port: u16,
    pub io_val: Vec<u8>,

    pub read_mem_offset: usize,
    pub write_mem_offset: usize,

    pub state: CpuState,
}

impl Cpu for MultipleCellCpu {
    type Error = TestCpuError;

    async fn read_memory(
        &mut self,
        gva: u64,
        bytes: &mut [u8],
        _is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        if gva == self.valid_gva + self.read_mem_offset as u64 {
            if self.mem_val.len() >= bytes.len() {
                bytes.copy_from_slice(self.mem_val.drain(0..bytes.len()).as_slice());
                self.read_mem_offset += bytes.len();
                Ok(())
            } else {
                Err(TestCpuError::BadLength)
            }
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn write_memory(
        &mut self,
        gva: u64,
        bytes: &[u8],
        _is_user_mode: bool,
    ) -> Result<(), Self::Error> {
        if gva == self.valid_gva + self.write_mem_offset as u64 {
            self.mem_val.extend_from_slice(bytes);
            self.write_mem_offset += bytes.len();
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn compare_and_write_memory(
        &mut self,
        gva: u64,
        _current: &[u8],
        new: &[u8],
        is_user_mode: bool,
    ) -> Result<bool, Self::Error> {
        // Memory is not concurrently mutable, so no need to compare.
        self.write_memory(gva, new, is_user_mode).await?;
        Ok(true)
    }

    async fn read_io(&mut self, io_port: u16, bytes: &mut [u8]) -> Result<(), Self::Error> {
        if io_port == self.valid_io_port {
            if self.io_val.len() >= bytes.len() {
                bytes.copy_from_slice(self.io_val.drain(0..bytes.len()).as_slice());
                Ok(())
            } else {
                Err(TestCpuError::BadLength)
            }
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    async fn write_io(&mut self, io_port: u16, bytes: &[u8]) -> Result<(), Self::Error> {
        if io_port == self.valid_io_port {
            self.io_val.extend_from_slice(bytes);
            Ok(())
        } else {
            Err(TestCpuError::BadAddress)
        }
    }

    fn gp(&mut self, reg: RegisterIndex) -> u64 {
        reg.apply_sizing(self.state.gps[reg.extended_index as usize])
    }

    fn gp_sign_extend(&mut self, reg: RegisterIndex) -> i64 {
        reg.apply_sizing_signed(self.state.gps[reg.extended_index as usize])
    }

    fn set_gp(&mut self, reg: RegisterIndex, v: u64) {
        let val = reg.apply_update(self.state.gps[reg.extended_index as usize], v);
        self.state.gps[reg.extended_index as usize] = val;
    }

    fn rip(&mut self) -> u64 {
        self.state.rip
    }

    fn set_rip(&mut self, v: u64) {
        self.state.rip = v
    }

    fn segment(&mut self, reg: Segment) -> SegmentRegister {
        self.state.segs[reg as usize]
    }

    fn efer(&mut self) -> u64 {
        self.state.efer
    }

    fn cr0(&mut self) -> u64 {
        self.state.cr0
    }

    fn rflags(&mut self) -> RFlags {
        self.state.rflags
    }
    fn set_rflags(&mut self, v: RFlags) {
        self.state.rflags = v
    }

    fn set_xmm(&mut self, _reg: usize, _value: u128) {
        todo!()
    }

    fn xmm(&mut self, _reg: usize) -> u128 {
        todo!()
    }
}

// When adding new tests, if the relevant instruction can be run locally,
// the below code can be modified and used to generate test values:
// #[cfg(target_arch = "x86_64")]
// #[test]
// #[ignore]
// fn get_values_and_flags() {
//     for (left, right) in [
//         (0x0u64, 0x0u64),
//         (0x64, 0x64),
//         (0x0, 0x1),
//         (0x1, 0x0),
//         (0xffffffffffffffff, 0x0),
//         (0xffffffffffffffff, 0xffffffff),
//         (0xffffffff, 0xffffffffffffffff),
//         (0xffffffff, 0xffffffff),
//         (0x7fffffffffffffff, 0x0),
//         (0x7fffffff, 0x0),
//         (0x0, 0x7fffffff),
//         (0x80000000, 0x7fffffff),
//         (0x7fffffff, 0x80000000),
//         (0x8000000000000000, 0x7fffffff),
//         (0x7fffffff, 0x8000000000000000),
//         (0x7fffffffffffffff, 0x7fffffffffffffff),
//         (0x8000000000000000, 0x7fffffffffffffff),
//         (0x8000000000000000, 0x8000000000000000),
//     ] {
//         let flags: u64;
//         let mut result = left;
//         unsafe {
//             std::arch::asm! {
//                 "or {left:e}, dword ptr [{right}]", // Change to whatever instruction you're testing.
//                 "pushf",
//                 "pop {flags}",
//                 left = inout(reg) result,
//                 right = in(reg) &right,
//                 flags = out(reg) flags,
//             };
//         }
//         let flags = flags & u64::from(RFLAGS_LOGIC_MASK); // Change mask if necessary.
//         println!("({left:#x}, {right:#x}, {result:#x}, {flags:#x}),");
//     }
// }

pub trait TestCpu: Debug + PartialEq<Self> + Cpu {
    fn new(rflags: RFlags) -> Self;
    fn state(&self) -> &CpuState;
    fn set_cr0(&mut self, v: u64);
    fn set_segment(&mut self, index: Segment, v: SegmentRegister);
}

impl<T: TestRegister> TestCpu for SingleCellCpu<T> {
    fn new(rflags: RFlags) -> Self {
        let seg = SegmentRegister {
            base: 0,
            limit: 0,
            attributes: SegmentAttributes::new().with_long(true),
            selector: 0,
        };
        let state = CpuState {
            gps: [0xbadc0ffee0ddf00d; 16],
            segs: [seg; 6],
            rip: 0,
            rflags,
            cr0: x86defs::X64_CR0_PE,
            efer: x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME,
        };
        SingleCellCpu {
            valid_gva: 0,
            mem_val: T::default(),
            valid_io_port: 0,
            io_val: 0,
            xmm: [0; 16],
            invert_after_read: false,
            state,
        }
    }
    fn state(&self) -> &CpuState {
        &self.state
    }

    fn set_cr0(&mut self, v: u64) {
        self.state.cr0 = v;
    }

    fn set_segment(&mut self, index: Segment, v: SegmentRegister) {
        self.state.segs[index as usize] = v;
    }
}

/// ignore register state when comparing CPUs
impl<T: TestRegister> PartialEq for SingleCellCpu<T> {
    fn eq(&self, other: &Self) -> bool {
        self.valid_gva == other.valid_gva
            && self.mem_val == other.mem_val
            && self.valid_io_port == other.valid_io_port
            && self.io_val == other.io_val
            && self.xmm == other.xmm
            && self.invert_after_read == other.invert_after_read
    }
}

impl TestCpu for MultipleCellCpu {
    fn new(rflags: RFlags) -> Self {
        let seg = SegmentRegister {
            base: 0,
            limit: 0,
            attributes: SegmentAttributes::new().with_long(true),
            selector: 0,
        };
        let state = CpuState {
            gps: [0xbadc0ffee0ddf00d; 16],
            segs: [seg; 6],
            rip: 0,
            rflags,
            cr0: x86defs::X64_CR0_PE,
            efer: x86defs::X64_EFER_LMA | x86defs::X64_EFER_LME,
        };
        MultipleCellCpu {
            valid_gva: 0,
            mem_val: Vec::<u8>::default(),
            valid_io_port: 0,
            io_val: Vec::<u8>::default(),
            read_mem_offset: 0,
            write_mem_offset: 0,
            state,
        }
    }
    fn state(&self) -> &CpuState {
        &self.state
    }

    fn set_cr0(&mut self, v: u64) {
        self.state.cr0 = v;
    }

    fn set_segment(&mut self, index: Segment, v: SegmentRegister) {
        self.state.segs[index as usize] = v;
    }
}

/// ignore register state when comparing CPUs
impl PartialEq for MultipleCellCpu {
    fn eq(&self, other: &Self) -> bool {
        self.valid_gva == other.valid_gva
            && self.mem_val == other.mem_val
            && self.valid_io_port == other.valid_io_port
            && self.io_val == other.io_val
            && self.read_mem_offset == other.read_mem_offset
            && self.write_mem_offset == other.write_mem_offset
    }
}

pub trait TestRegister
where
    Self: PartialEq<Self> + Debug + Default,
{
    // TODO: This really should be just a constant SIZE, but const generic support isn't good enough yet.
    type Array: AsRef<[u8]> + AsMut<[u8]>;
    fn from_le_bytes(bytes: Self::Array) -> Self;
    fn to_le_bytes(&self) -> Self::Array;
}
impl TestRegister for u64 {
    type Array = [u8; 8];
    fn from_le_bytes(bytes: Self::Array) -> Self {
        Self::from_le_bytes(bytes)
    }
    fn to_le_bytes(&self) -> Self::Array {
        (*self).to_le_bytes()
    }
}
impl TestRegister for u128 {
    type Array = [u8; 16];
    fn from_le_bytes(bytes: Self::Array) -> Self {
        Self::from_le_bytes(bytes)
    }
    fn to_le_bytes(&self) -> Self::Array {
        (*self).to_le_bytes()
    }
}
