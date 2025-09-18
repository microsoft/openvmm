// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Tests for hypercall handling.

use super::support::HypercallDispatch;
use super::support::HypercallParameters;
use super::support::RepHypercall;
use super::support::SimpleHypercall;
use super::support::VariableHypercall;
use super::support::VariableRepHypercall;
use super::support::VtlHypercall;
use super::*;
use crate::Arm64RegisterIo;
use crate::Arm64RegisterState;
use crate::X64RegisterIo;
use crate::X64RegisterState;
use guestmem::GuestMemory;
use guestmem::PAGE_SIZE;
use hvdef::HV_PAGE_SIZE_USIZE;
use hvdef::HvError;
use hvdef::HvResult;
use hvdef::hypercall::Control;
use hvdef::hypercall::HypercallOutput;
use open_enum::open_enum;
use sparse_mmap::SparseMapping;
use std::vec;
use test_with_tracing::test;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

// A useful base pattern to fill into hypercall input and output.
const FILL_PATTERN: u64 = 0x123456789abcdef0;
const BACK_PATTERN: u64 = 0x1919191919191919;
const BACK_BYTE: u8 = 0x19;

// `[u64; 2]` buffer aligned to 16 bytes for hypercall inputs.
#[repr(C, align(16))]
#[derive(Copy, Clone)]
struct TestHypercallAlignedBuf128<const N: usize>([[u64; 2]; N]);

impl<const N: usize> TestHypercallAlignedBuf128<N> {
    fn new_zeroed() -> Self {
        Self([[0, 0]; N])
    }
}

type TestHypercallAlignedPage = TestHypercallAlignedBuf128<{ HV_PAGE_SIZE_USIZE / 16 }>;

/// This trait is essentially the other side of HypercallIo. HypercallIo refers to the operations
/// the hypercall parser/implementation needs to successfully get input register state and set
/// output register state. TestHypercallIo refers to the operations the hypercall invoker would need
/// to set hypercall input register state and get the output register state.
pub trait TestHypercallIo: HypercallIo {
    fn set_control(&mut self, control: u64);
    fn get_result(&mut self) -> u64;
    fn set_input_gpa(&mut self, gpa: u64);
    fn set_output_gpa(&mut self, gpa: u64);
    fn set_fast_input(&mut self, buf: &[[u64; 2]]);
    fn get_fast_output(&mut self, input_register_pairs: usize, buf: &mut [[u64; 2]]);
    fn get_modified_mask(&self) -> u64;
    fn clear_modified_mask(&mut self);
    fn get_io_register_mask(&self) -> u64;
    fn get_name(&self) -> String;
    fn set_vtl_input(&mut self, vtl_input: u64);
    fn auto_advance_ip(&mut self);
}

//  Sub-trait for TestHypercallIo that also requires the ability to be used as a HypercallHandler.
trait TestHandlerIo<'a>: TestHypercallIo + AsHandler<TestHypercallHandler<'a>> + 'a {}
impl<'a, T: TestHypercallIo + AsHandler<TestHypercallHandler<'a>> + 'a> TestHandlerIo<'a> for T {}

// Implementation of AsHandler, HypercallIo and TestHypercallIo for the Box dyn trait versions of
// TestHandlerIo.
impl<'a> AsHandler<TestHypercallHandler<'a>> for Box<dyn TestHandlerIo<'a>> {
    fn as_handler(&mut self) -> &mut TestHypercallHandler<'a> {
        (**self).as_handler()
    }
}

impl HypercallIo for Box<dyn TestHandlerIo<'_>> {
    fn advance_ip(&mut self) {
        (**self).advance_ip();
    }

    fn retry(&mut self, control: u64) {
        (**self).retry(control);
    }

    fn control(&mut self) -> u64 {
        (**self).control()
    }

    fn input_gpa(&mut self) -> u64 {
        (**self).input_gpa()
    }

    fn output_gpa(&mut self) -> u64 {
        (**self).output_gpa()
    }

    fn fast_register_pair_count(&mut self) -> usize {
        (**self).fast_register_pair_count()
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        (**self).extended_fast_hypercalls_ok()
    }

    fn fast_input(&mut self, buf: &mut [[u64; 2]], output_register_pairs: usize) -> usize {
        (**self).fast_input(buf, output_register_pairs)
    }

    fn fast_output(&mut self, starting_pair_index: usize, buf: &[[u64; 2]]) {
        (**self).fast_output(starting_pair_index, buf);
    }

    fn vtl_input(&mut self) -> u64 {
        (**self).vtl_input()
    }

    fn set_result(&mut self, n: u64) {
        (**self).set_result(n);
    }

    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        (**self).fast_regs(starting_pair_index, buf);
    }
}

impl TestHypercallIo for Box<dyn TestHandlerIo<'_>> {
    fn set_control(&mut self, control: u64) {
        (**self).set_control(control);
    }

    fn get_result(&mut self) -> u64 {
        (**self).get_result()
    }

    fn set_input_gpa(&mut self, gpa: u64) {
        (**self).set_input_gpa(gpa);
    }

    fn set_output_gpa(&mut self, gpa: u64) {
        (**self).set_output_gpa(gpa);
    }

    fn set_fast_input(&mut self, buf: &[[u64; 2]]) {
        (**self).set_fast_input(buf);
    }

    fn get_fast_output(&mut self, input_register_pairs: usize, buf: &mut [[u64; 2]]) {
        (**self).get_fast_output(input_register_pairs, buf);
    }

    fn get_modified_mask(&self) -> u64 {
        (**self).get_modified_mask()
    }

    fn clear_modified_mask(&mut self) {
        (**self).clear_modified_mask();
    }

    fn get_io_register_mask(&self) -> u64 {
        (**self).get_io_register_mask()
    }

    fn get_name(&self) -> String {
        (**self).get_name()
    }

    fn set_vtl_input(&mut self, vtl_input: u64) {
        (**self).set_vtl_input(vtl_input);
    }

    fn auto_advance_ip(&mut self) {
        (**self).auto_advance_ip();
    }
}

impl<T: TestHypercallIo> TestHypercallIo for &mut T {
    fn set_control(&mut self, control: u64) {
        (**self).set_control(control);
    }

    fn get_result(&mut self) -> u64 {
        (**self).get_result()
    }

    fn set_input_gpa(&mut self, gpa: u64) {
        (**self).set_input_gpa(gpa);
    }

    fn set_output_gpa(&mut self, gpa: u64) {
        (**self).set_output_gpa(gpa);
    }

    fn set_fast_input(&mut self, buf: &[[u64; 2]]) {
        (**self).set_fast_input(buf);
    }

    fn get_fast_output(&mut self, input_register_pairs: usize, buf: &mut [[u64; 2]]) {
        (**self).get_fast_output(input_register_pairs, buf);
    }

    fn get_modified_mask(&self) -> u64 {
        (**self).get_modified_mask()
    }

    fn clear_modified_mask(&mut self) {
        (**self).clear_modified_mask();
    }

    fn get_io_register_mask(&self) -> u64 {
        (**self).get_io_register_mask()
    }

    fn get_name(&self) -> String {
        (**self).get_name()
    }

    fn set_vtl_input(&mut self, vtl_input: u64) {
        (**self).set_vtl_input(vtl_input);
    }

    fn auto_advance_ip(&mut self) {
        (**self).auto_advance_ip();
    }
}

// Closure/Function type for generating a TestHandlerIo object from a mutable reference to a
// TestHypercallHandler.
type TestHandlerIoBuilder =
    Box<dyn for<'a, 'b> Fn(&'a mut TestHypercallHandler<'b>) -> Box<dyn TestHandlerIo<'b> + 'a>>;

/// Additional test-only interface to extend architecture specific *RegisterState.
pub trait TestRegisterState {
    /// Returns a bit mask of the registers that have been modified. 1 bit per 64 bit register
    /// accessed (128 bit registers therefore use 2 bits). The position of each register in the
    /// bitmask must be stable.
    fn get_modified_mask(&self) -> u64;
    /// Clears the modified mask.
    fn clear_modified_mask(&mut self);
}

struct TestHypercallHandler<'a> {
    ctrl: &'a mut TestController,
    vp: &'a mut TestVp,
}

// Test VP object that implements the register state for any desired architecture.
struct TestVp {
    gp_regs: [u64; 18],
    xmms: [u128; 16],
    reg_changed_mask: u64,
    invalid_opcode: bool,
    ip: u64,
}

#[derive(Copy, Clone, Debug)]
enum SimpleResult {
    Success,
    Failure(HvError),
}

#[derive(Copy, Clone, Debug)]
enum RepResult {
    Success(usize),
    Failure(HvError, usize), // Error, total processed rep_count
}

#[derive(Copy, Clone, Debug)]
enum TestResult {
    Simple(SimpleResult),
    Rep(RepResult),
    Vtl(bool),
}

impl TestResult {
    fn is_timeout(&self) -> bool {
        matches!(
            self,
            TestResult::Simple(SimpleResult::Failure(HvError::Timeout))
                | TestResult::Rep(RepResult::Failure(HvError::Timeout, _))
        )
    }

    fn expected_elements_processed(&self) -> usize {
        match self {
            TestResult::Simple(SimpleResult::Success) => 0,
            TestResult::Simple(SimpleResult::Failure(_)) => 0,
            TestResult::Rep(RepResult::Success(rep_count)) => *rep_count,
            TestResult::Rep(RepResult::Failure(_, rep_count)) => *rep_count,
            _ => panic!("Should not be invoked for VTL"),
        }
    }
}

impl From<TestResult> for HypercallOutput {
    fn from(result: TestResult) -> HypercallOutput {
        match result {
            TestResult::Simple(SimpleResult::Success) => HypercallOutput::new(),
            TestResult::Simple(SimpleResult::Failure(err)) => {
                HypercallOutput::new().with_call_status(Err(err).into())
            }
            TestResult::Rep(RepResult::Success(rep_count)) => {
                HypercallOutput::new().with_elements_processed(rep_count)
            }
            TestResult::Rep(RepResult::Failure(err, rep_count)) => HypercallOutput::new()
                .with_call_status(Err(err).into())
                .with_elements_processed(rep_count),
            _ => panic!("Should not be invoked for VTL"),
        }
    }
}

// Test controller object for vtl switch calls.
#[derive(Default, Debug)]
struct VtlTestController {
    invoked: bool,
    lower_vtl_enabled: bool,
}

// Test controller object that implements the hypercalls.
#[derive(Debug)]
struct TestController {
    test_result: TestResult,
    reps: Option<(usize, usize)>, // rep_start, rep_count
    var_size: Option<usize>,
    vtl_controller: VtlTestController,
}

open_enum! {
    #[derive(IntoBytes, Immutable, KnownLayout, FromBytes)]
    enum TestHypercallCode: u16 {
        #![expect(non_upper_case_globals)]
        CallSimpleNoOutput = 0x1001,
        CallSimple = 0x1002,
        CallRepNoOutput = 0x1003,
        CallRep = 0x1004,
        CallVariable = 0x1005,
        CallNull = 0x1006,
        CallVariableNoOutput = 0x1007,
        CallVariableRep = 0x1008,
        CallVtl = 0x1009,

        // This must never be added to the dispatcher - reserved to test unimplemented calls.
        CallReserved = 0xFFFF,
    }
}

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct TestInput([u8; 16]);

#[repr(C)]
#[derive(Copy, Clone, Debug, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct TestOutput([u8; 16]);

// Simple hypercall with no input or output.
type TestNull = SimpleHypercall<(), (), { TestHypercallCode::CallNull.0 }>;
impl HypercallDispatch<TestNull> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestNull::run(params, |()| self.ctrl.simple_null())
    }
}

// Simple hypercall with input, but no output.
type TestSimpleNoOutput =
    SimpleHypercall<TestInput, (), { TestHypercallCode::CallSimpleNoOutput.0 }>;

impl HypercallDispatch<TestSimpleNoOutput> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestSimpleNoOutput::run(params, |input| self.ctrl.simple_no_output(input))
    }
}

// Simple hypercall with input and output.
type TestSimple = SimpleHypercall<TestInput, TestOutput, { TestHypercallCode::CallSimple.0 }>;

impl HypercallDispatch<TestSimple> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestSimple::run(params, |input| self.ctrl.simple(input))
    }
}

// Rep hypercall with no output.
type TestRepNoOutput = RepHypercall<TestInput, u64, (), { TestHypercallCode::CallRepNoOutput.0 }>;

impl HypercallDispatch<TestRepNoOutput> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestRepNoOutput::run(params, |header, input, _output| {
            self.ctrl.rep_no_output(header, input)
        })
    }
}

// Rep hypercall with input and output.
type TestRep = RepHypercall<TestInput, u64, u64, { TestHypercallCode::CallRep.0 }>;
impl HypercallDispatch<TestRep> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestRep::run(params, |header, input, output| {
            self.ctrl.rep(header, input, output)
        })
    }
}

// Simple variable hypercall with no output.
type TestVariableNoOutput =
    VariableHypercall<TestInput, (), { TestHypercallCode::CallVariableNoOutput.0 }>;

impl HypercallDispatch<TestVariableNoOutput> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestVariableNoOutput::run(params, |input, var_header| {
            self.ctrl.variable_no_output(input, var_header)
        })
    }
}

// Simple variable hypercall with input and output.
type TestVariable = VariableHypercall<TestInput, TestOutput, { TestHypercallCode::CallVariable.0 }>;

impl HypercallDispatch<TestVariable> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestVariable::run(params, |input, var_header| {
            self.ctrl.variable(input, var_header)
        })
    }
}

// Rep variable hypercall with input and output.
type TestVariableRep =
    VariableRepHypercall<TestInput, u64, u64, { TestHypercallCode::CallVariableRep.0 }>;

impl HypercallDispatch<TestVariableRep> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        TestVariableRep::run(params, |header, var_header, input, output| {
            self.ctrl.variable_rep(header, var_header, input, output)
        })
    }
}

// VTL call.
pub type TestVtl = VtlHypercall<{ TestHypercallCode::CallVtl.0 }>;

impl HypercallDispatch<TestVtl> for TestHypercallHandler<'_> {
    fn dispatch(&mut self, params: HypercallParameters<'_>) -> HypercallOutput {
        let (input, _) = TestVtl::parse(params);
        self.ctrl.vtl_switch(input);
        HypercallOutput::SUCCESS
    }
}

// VtlReturn call.
impl VtlReturn for TestHypercallHandler<'_> {
    fn is_vtl_return_allowed(&self) -> bool {
        self.ctrl.vtl_controller.lower_vtl_enabled
    }

    fn vtl_return(&mut self, vtl_input: bool) {
        self.ctrl.vtl_switch(vtl_input.into());
    }
}

impl VtlSwitchOps for TestHypercallHandler<'_> {
    fn advance_ip(&mut self) {
        X64RegisterIo::new(self, true).advance_ip();
    }

    fn inject_invalid_opcode_fault(&mut self) {
        self.vp.inject_invalid_opcode_fault();
    }
}

impl<'a> TestHypercallHandler<'a> {
    const DISPATCHER: Dispatcher<Self> = crate::dispatcher!(
        Self,
        [
            TestNull,
            TestSimpleNoOutput,
            TestSimple,
            TestRepNoOutput,
            TestRep,
            TestVariableNoOutput,
            TestVariable,
            TestVariableRep,
            TestVtl,
            // VtlReturn uses rather more custom handling. Add additional tests here using the
            // normal (non-test) trait.
            HvVtlReturn,
        ]
    );

    fn new(ctrl: &'a mut TestController, vp: &'a mut TestVp) -> TestHypercallHandler<'a> {
        Self { ctrl, vp }
    }
}

impl Default for TestVp {
    fn default() -> Self {
        Self {
            gp_regs: [BACK_PATTERN; 18],
            xmms: [(BACK_PATTERN as u128) << 64 | BACK_PATTERN as u128; 16],
            reg_changed_mask: 0,
            invalid_opcode: false,
            ip: Self::INITIAL_IP,
        }
    }
}

impl TestVp {
    const INITIAL_IP: u64 = 0x1234;
    fn instruction_pointer(&self) -> u64 {
        self.ip
    }

    fn set_instruction_pointer(&mut self, ip: u64) {
        self.ip = ip;
    }

    fn gp(&self, n: usize) -> u64 {
        self.gp_regs[n]
    }

    fn set_gp(&mut self, n: usize, value: u64) {
        self.gp_regs[n] = value;
        self.reg_changed_mask |= 1 << n;
    }

    fn xmm(&self, n: usize) -> u128 {
        self.xmms[n]
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.xmms[n] = value;
        let n = 2 * n + self.gp_regs.len();
        self.reg_changed_mask |= 3 << n;
    }

    fn get_modified_mask(&self) -> u64 {
        self.reg_changed_mask
    }

    fn clear_modified_mask(&mut self) {
        self.reg_changed_mask = 0;
    }

    fn inject_invalid_opcode_fault(&mut self) {
        assert!(!self.invalid_opcode);
        self.invalid_opcode = true;
    }
}

impl X64RegisterState for TestHypercallHandler<'_> {
    fn rip(&mut self) -> u64 {
        self.vp.instruction_pointer()
    }

    fn set_rip(&mut self, rip: u64) {
        self.vp.set_instruction_pointer(rip);
    }

    fn gp(&mut self, n: X64HypercallRegister) -> u64 {
        self.vp.gp(n as usize)
    }

    fn set_gp(&mut self, n: X64HypercallRegister, value: u64) {
        self.vp.set_gp(n as usize, value);
    }

    fn xmm(&mut self, n: usize) -> u128 {
        self.vp.xmm(n)
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        self.vp.set_xmm(n, value);
    }
}

impl Arm64RegisterState for TestHypercallHandler<'_> {
    fn pc(&mut self) -> u64 {
        self.vp.instruction_pointer()
    }

    fn set_pc(&mut self, pc: u64) {
        self.vp.set_instruction_pointer(pc);
    }

    fn x(&mut self, n: u8) -> u64 {
        self.vp.gp(n as usize)
    }

    fn set_x(&mut self, n: u8, value: u64) {
        self.vp.set_gp(n as usize, value);
    }
}

impl TestRegisterState for TestHypercallHandler<'_> {
    fn get_modified_mask(&self) -> u64 {
        self.vp.get_modified_mask()
    }

    fn clear_modified_mask(&mut self) {
        self.vp.clear_modified_mask()
    }
}

impl<T: TestRegisterState> TestRegisterState for &'_ mut T {
    fn get_modified_mask(&self) -> u64 {
        (**self).get_modified_mask()
    }

    fn clear_modified_mask(&mut self) {
        (**self).clear_modified_mask()
    }
}

struct TestMemory {
    gm: GuestMemory,
    input_allocated: bool,
    output_allocated: bool,
    internal_buffers: [TestHypercallAlignedPage; 2],
}

impl TestMemory {
    const INPUT_BASE: usize = PAGE_SIZE;
    const OUTPUT_BASE: usize = 3 * PAGE_SIZE;
    const IN_INDEX: usize = 0;
    const OUT_INDEX: usize = 1;

    fn new(input: bool, output: bool) -> Self {
        // Map 5 pages of memory, but only back two of them, so that there are guard pages
        // before and after each backed page.
        let mapping = SparseMapping::new(5 * PAGE_SIZE).unwrap();
        if input {
            mapping.alloc(Self::INPUT_BASE, PAGE_SIZE).unwrap()
        };

        if output {
            mapping.alloc(Self::OUTPUT_BASE, PAGE_SIZE).unwrap();
        }

        let mut buffers = [TestHypercallAlignedPage::new_zeroed(); 2];
        for buffer in buffers.iter_mut() {
            let buffer = buffer.0.as_mut_bytes();
            buffer.fill(BACK_BYTE);
        }

        Self {
            gm: GuestMemory::new("hv1_hypercall-test", mapping),
            input_allocated: input,
            output_allocated: output,
            internal_buffers: buffers,
        }
    }

    fn buffers_to_gm(&self) {
        if self.input_allocated {
            self.gm
                .write_at(
                    TestMemory::INPUT_BASE as u64,
                    self.internal_buffers[Self::IN_INDEX].0.as_bytes(),
                )
                .unwrap();
        }

        if self.output_allocated {
            self.gm
                .write_at(
                    TestMemory::OUTPUT_BASE as u64,
                    self.internal_buffers[Self::OUT_INDEX].0.as_bytes(),
                )
                .unwrap();
        }
    }

    fn gm_to_buffers(&mut self) {
        if self.input_allocated {
            self.gm
                .read_at(
                    TestMemory::INPUT_BASE as u64,
                    self.internal_buffers[Self::IN_INDEX].0.as_mut_bytes(),
                )
                .unwrap();
        }

        if self.output_allocated {
            self.gm
                .read_at(
                    TestMemory::OUTPUT_BASE as u64,
                    self.internal_buffers[Self::OUT_INDEX].0.as_mut_bytes(),
                )
                .unwrap();
        }
    }
}

impl TestController {
    fn new(test_result: TestResult) -> Self {
        Self {
            test_result,
            reps: None,
            var_size: None,
            vtl_controller: VtlTestController::default(),
        }
    }

    fn with_reps(mut self, rep_start: usize, rep_count: usize) -> Self {
        self.reps = Some((rep_start, rep_count));
        self
    }

    fn with_var_size(mut self, var_size: usize) -> Self {
        self.var_size = Some(var_size);
        self
    }

    fn simple_null(&self) -> HvResult<()> {
        println!("simple_null");
        match self.test_result {
            TestResult::Simple(SimpleResult::Success) => Ok(()),
            TestResult::Simple(SimpleResult::Failure(e)) => Err(e),
            _ => panic!("Unexpected test result"),
        }
    }

    fn simple_no_output<InputT>(&self, input_header: &InputT) -> HvResult<()>
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("simple_no_output");
        match self.test_result {
            TestResult::Simple(SimpleResult::Success) => {
                assert_eq!(
                    input_header.as_bytes(),
                    Self::generate_test_input::<InputT>().as_bytes()
                );
                Ok(())
            }
            TestResult::Simple(SimpleResult::Failure(e)) => Err(e),
            _ => panic!("Unexpected test result"),
        }
    }

    fn simple<InputT, OutputT>(&self, input: &InputT) -> HvResult<OutputT>
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        OutputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("simple");
        match self.test_result {
            TestResult::Simple(SimpleResult::Success) => {
                assert_eq!(
                    input.as_bytes(),
                    Self::generate_test_input::<InputT>().as_bytes()
                );

                Ok(Self::generate_test_output::<OutputT>())
            }
            TestResult::Simple(SimpleResult::Failure(e)) => Err(e),
            _ => panic!("Unexpected test result"),
        }
    }

    fn rep_no_output<InputT, InRepT>(&self, header: &InputT, input: &[InRepT]) -> HvRepResult
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        InRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("rep_no_output");
        let (rep_start, rep_count) = self.reps.unwrap();
        assert_eq!(
            header.as_bytes(),
            Self::generate_test_input::<InputT>().as_bytes()
        );
        assert_eq!(
            input.as_bytes(),
            Self::generate_input_reps::<InRepT>(rep_count)[rep_start..].as_bytes()
        );

        match self.test_result {
            TestResult::Rep(RepResult::Success(_)) => Ok(()),
            TestResult::Rep(RepResult::Failure(e, reps)) => Err((e, reps - rep_start)),
            _ => panic!("Unexpected test result"),
        }
    }

    fn rep<InputT, InRepT, OutRepT>(
        &self,
        header: &InputT,
        input: &[InRepT],
        output: &mut [OutRepT],
    ) -> HvRepResult
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        InRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        OutRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("rep");
        let (rep_start, rep_count) = self.reps.unwrap();
        assert_eq!(
            header.as_bytes(),
            Self::generate_test_input::<InputT>().as_bytes()
        );
        assert_eq!(
            input.as_bytes(),
            Self::generate_input_reps::<InRepT>(rep_count)[rep_start..].as_bytes()
        );

        let generated_output = Self::generate_output_reps::<OutRepT>(rep_count);
        match self.test_result {
            TestResult::Rep(RepResult::Success(_)) => {
                output.copy_from_slice(&generated_output[rep_start..]);
                Ok(())
            }
            TestResult::Rep(RepResult::Failure(e, reps)) => {
                output[..reps - rep_start].copy_from_slice(&generated_output[rep_start..reps]);
                Err((e, reps - rep_start))
            }
            _ => panic!("Unexpected test result"),
        }
    }

    fn variable_no_output<InputT>(&self, input: &InputT, var_header: &[u64]) -> HvResult<()>
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("simple_variable_no_output");
        match self.test_result {
            TestResult::Simple(SimpleResult::Success) => {
                assert_eq!(
                    input.as_bytes(),
                    Self::generate_test_input::<InputT>().as_bytes()
                );
                assert_eq!(
                    var_header.as_bytes(),
                    Self::generate_var_header(self.var_size.unwrap()).as_bytes()
                );

                Ok(())
            }
            TestResult::Simple(SimpleResult::Failure(e)) => Err(e),
            _ => panic!("Unexpected test result"),
        }
    }

    fn variable<InputT, OutputT>(&self, input: &InputT, var_header: &[u64]) -> HvResult<OutputT>
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        OutputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("simple_variable");
        match self.test_result {
            TestResult::Simple(SimpleResult::Success) => {
                assert_eq!(
                    input.as_bytes(),
                    Self::generate_test_input::<InputT>().as_bytes()
                );
                assert_eq!(
                    var_header.as_bytes(),
                    Self::generate_var_header(self.var_size.unwrap()).as_bytes()
                );

                Ok(Self::generate_test_output::<OutputT>())
            }
            TestResult::Simple(SimpleResult::Failure(e)) => Err(e),
            _ => panic!("Unexpected test result"),
        }
    }

    fn variable_rep<InputT, InRepT, OutRepT>(
        &self,
        header: &InputT,
        var_header: &[u64],
        input: &[InRepT],
        output: &mut [OutRepT],
    ) -> HvRepResult
    where
        InputT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        InRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
        OutRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        println!("var_rep");
        let (rep_start, rep_count) = self.reps.unwrap();
        assert_eq!(
            header.as_bytes(),
            Self::generate_test_input::<InputT>().as_bytes()
        );
        assert_eq!(
            var_header.as_bytes(),
            Self::generate_var_header(self.var_size.unwrap()).as_bytes()
        );
        assert_eq!(
            input.as_bytes(),
            Self::generate_input_reps::<InRepT>(rep_count)[rep_start..].as_bytes()
        );

        let generated_output = Self::generate_output_reps::<OutRepT>(rep_count);
        output.copy_from_slice(&generated_output[rep_start..]);

        match self.test_result {
            TestResult::Rep(RepResult::Success(_)) => Ok(()),
            TestResult::Rep(RepResult::Failure(e, reps)) => Err((e, reps - rep_start)),
            _ => panic!("Unexpected test result"),
        }
    }

    fn vtl_switch(&mut self, _vtl_input: u64) {
        self.vtl_controller.invoked = true;
    }

    fn generate_test_input<InputHeaderT>() -> InputHeaderT
    where
        InputHeaderT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        assert!(size_of::<InputHeaderT>().is_multiple_of(8));
        *InputHeaderT::ref_from_bytes(vec![FILL_PATTERN; size_of::<TestInput>() / 8].as_bytes())
            .unwrap()
    }

    fn generate_var_header(size: usize) -> Vec<u8> {
        let mut x = vec![FILL_PATTERN + 1; size.div_ceil(8)].as_bytes().to_vec();
        x.truncate(size);
        x
    }

    fn generate_input_reps<InRepT>(rep_count: usize) -> Vec<InRepT>
    where
        InRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        let size = rep_count * size_of::<InRepT>();
        let pattern_count = size.div_ceil(8);
        let mut reps = Vec::new();
        for i in 0..pattern_count {
            reps.push(FILL_PATTERN + 2 + i as u64);
        }

        let (reps, _) = <[InRepT]>::ref_from_prefix_with_elems(reps.as_bytes(), rep_count).unwrap();
        reps.to_vec()
    }

    fn generate_test_output<OutputT>() -> OutputT
    where
        OutputT: IntoBytes + FromBytes + FromZeros + Sized + Copy + Immutable + KnownLayout,
    {
        assert!(size_of::<TestOutput>().is_multiple_of(16));
        *OutputT::ref_from_bytes(vec![!FILL_PATTERN; size_of::<TestOutput>() / 8].as_bytes())
            .unwrap()
    }

    fn generate_output_reps<OutRepT>(rep_count: usize) -> Vec<OutRepT>
    where
        OutRepT: IntoBytes + FromBytes + Sized + Copy + Immutable + KnownLayout,
    {
        let size = rep_count * size_of::<OutRepT>();
        let pattern_count = size.div_ceil(8);
        let mut reps = Vec::new();
        for i in 0..pattern_count {
            reps.push(!FILL_PATTERN - 2 - i as u64);
        }

        let (reps, _) =
            <[OutRepT]>::ref_from_prefix_with_elems(reps.as_bytes(), rep_count).unwrap();
        reps.to_vec()
    }
}

// Hypercall architecture ABIs.
#[derive(Clone, Copy)]
enum TestHypercallAbi {
    X64 { is_64bit: bool },
    Aarch64 { pre_advanced: bool, smccc: bool },
}

impl TestHypercallAbi {
    fn extended_fast_hypercalls_ok(&self) -> bool {
        match self {
            TestHypercallAbi::X64 { is_64bit } => *is_64bit,
            _ => true,
        }
    }

    fn max_fast_output_size(&self) -> usize {
        match self {
            TestHypercallAbi::X64 { is_64bit: true } => 112,
            TestHypercallAbi::X64 { is_64bit: false } => 0,
            TestHypercallAbi::Aarch64 { .. } => 128,
        }
    }

    fn io_builder(&self) -> TestHandlerIoBuilder {
        match *self {
            TestHypercallAbi::X64 { is_64bit } => {
                Box::new(move |h| Box::new(X64RegisterIo::new(h, is_64bit)))
            }
            TestHypercallAbi::Aarch64 {
                pre_advanced,
                smccc,
            } => Box::new(move |h| Box::new(Arm64RegisterIo::new(h, pre_advanced, smccc))),
        }
    }

    const LIST: &'static [Self] = &[
        TestHypercallAbi::X64 { is_64bit: false },
        TestHypercallAbi::X64 { is_64bit: true },
        TestHypercallAbi::Aarch64 {
            pre_advanced: false,
            smccc: false,
        },
        TestHypercallAbi::Aarch64 {
            pre_advanced: false,
            smccc: true,
        },
        TestHypercallAbi::Aarch64 {
            pre_advanced: true,
            smccc: false,
        },
        TestHypercallAbi::Aarch64 {
            pre_advanced: true,
            smccc: true,
        },
    ];
}

// Common test parameters for hypercall tests.
#[derive(Clone, Copy)]
struct TestParams {
    in_offset: Option<usize>,
    out_offset: Option<usize>,
    fast: bool,
    input_control: Option<Control>,
    test_result: TestResult,
    rep_start: Option<usize>,
    rep_count: Option<usize>,
    var_size: Option<usize>,
    abi: Option<TestHypercallAbi>,
}

impl TestParams {
    fn new(test_result: TestResult) -> Self {
        Self {
            in_offset: None,
            out_offset: None,
            fast: false,
            input_control: None,
            test_result,
            rep_start: None,
            rep_count: None,
            var_size: None,
            abi: None,
        }
    }

    fn with_abi(mut self, abi: TestHypercallAbi) -> Self {
        self.abi = Some(abi);
        self
    }

    fn with_fast(mut self, fast: bool) -> Self {
        self.fast = fast;
        self
    }

    fn with_in_offset(mut self, in_offset: usize) -> Self {
        self.in_offset = Some(in_offset);
        self
    }

    fn with_out_offset(mut self, out_offset: usize) -> Self {
        self.out_offset = Some(out_offset);
        self
    }

    fn with_input_control(mut self, input_control: Control) -> Self {
        self.input_control = Some(input_control);
        self
    }

    fn with_rep_start(mut self, rep_start: usize) -> Self {
        self.rep_start = Some(rep_start);
        self
    }
    fn with_rep_count(mut self, rep_count: usize) -> Self {
        self.rep_count = Some(rep_count);
        self
    }

    fn with_var_size(mut self, var_size: usize) -> Self {
        self.var_size = Some(var_size);
        self
    }

    fn io_builder(&self) -> TestHandlerIoBuilder {
        self.abi.unwrap().io_builder()
    }

    fn with_result_reps(mut self, rep_count: usize) -> Self {
        self.test_result = match self.test_result {
            TestResult::Rep(RepResult::Success(_)) => {
                TestResult::Rep(RepResult::Success(rep_count))
            }
            TestResult::Rep(RepResult::Failure(e, _)) => {
                TestResult::Rep(RepResult::Failure(e, rep_count))
            }
            _ => self.test_result,
        };

        self
    }
}

impl TestParams {
    // Based on the expected test result whether the input page is expected to be accessed (and
    // should be mapped) into guest memory.
    fn map_input(&self) -> bool {
        if self.fast {
            return false;
        }

        !matches!(self.test_result,
        TestResult::Simple(SimpleResult::Failure(e))
        | TestResult::Rep(RepResult::Failure(e, _))
            if matches!(
                e,
                HvError::InvalidAlignment
                    | HvError::InvalidHypercallCode
                    | HvError::InvalidHypercallInput
            ))
    }

    // Based on the expected test result whether the output page is expected to be accessed (and
    // should be mapped) into guest memory.
    fn map_output(&self) -> bool {
        !self.fast
            && match self.test_result {
                TestResult::Simple(SimpleResult::Success)
                | TestResult::Rep(RepResult::Success(_)) => true,

                TestResult::Simple(SimpleResult::Failure(e))
                | TestResult::Rep(RepResult::Failure(e, _))
                    if matches!(
                        e,
                        HvError::InvalidAlignment
                            | HvError::InvalidHypercallCode
                            | HvError::InvalidHypercallInput
                    ) =>
                {
                    false
                }

                TestResult::Rep(RepResult::Failure(_, rep_count)) if rep_count > 0 => true,
                _ => false,
            }
    }
}

// Verify whether the result of the hypercall matches the expected result stored in TestParams.
fn check_test_result(test_params: &TestParams, result: HypercallOutput, control: Control) {
    if !test_params.test_result.is_timeout() {
        assert_eq!(
            u64::from(result),
            u64::from(HypercallOutput::from(test_params.test_result)),
            "actual result: {:?} desired result: {:?}",
            result,
            HypercallOutput::from(test_params.test_result)
        );
    } else {
        let reps = match test_params.test_result {
            TestResult::Rep(RepResult::Failure(HvError::Timeout, reps)) => reps,
            _ => 0,
        };

        assert_eq!({ control.rep_start() }, reps);
    }
}

struct InvokerParams<'a> {
    ctrl: &'a mut TestController,
    vp: &'a mut TestVp,
    test_mem: &'a mut TestMemory,
    fast: bool,
    in_offset: usize,
    out_offset: usize,
    input_control: Option<Control>,
    io_gen: TestHandlerIoBuilder,
}

impl<'a> InvokerParams<'a> {
    fn new(
        ctrl: &'a mut TestController,
        vp: &'a mut TestVp,
        test_mem: &'a mut TestMemory,
        test_params: &TestParams,
    ) -> Self {
        Self {
            ctrl,
            vp,
            test_mem,
            fast: test_params.fast,
            in_offset: test_params.in_offset.unwrap_or(0),
            out_offset: test_params.out_offset.unwrap_or(0),
            input_control: test_params.input_control,
            io_gen: test_params.io_builder(),
        }
    }
}

// Generic routine to invoke a hypercall with the specified input and output.
fn invoke_hypercall<InputT, InRepT, OutputT, OutRepT>(
    params: InvokerParams<'_>,
    input_header: &InputT,
    var_header: &[u8],
    input_reps: &[InRepT],
    output: &mut OutputT,
    output_reps: &mut [OutRepT],
) -> (HypercallOutput, Control)
where
    InputT: IntoBytes + FromBytes + Sized + Immutable + KnownLayout,
    InRepT: IntoBytes + FromBytes + Sized + Immutable + KnownLayout,
    OutputT: IntoBytes + FromBytes + Sized + Immutable + KnownLayout,
    OutRepT: IntoBytes + FromBytes + Sized + Immutable + KnownLayout,
{
    assert!(size_of::<InputT>().is_multiple_of(8));
    assert!(size_of::<OutputT>().is_multiple_of(8));
    assert!(var_header.len().is_multiple_of(8));
    assert!(params.in_offset < PAGE_SIZE);
    assert!(params.out_offset < PAGE_SIZE);
    assert!(size_of::<OutputT>() == 0 || output_reps.is_empty());

    // If the caller overrides the input control, the correct value of fast should be specified in
    // the override.
    assert!(
        params
            .input_control
            .unwrap_or(Control::new().with_fast(params.fast))
            .fast()
            == params.fast
    );

    let ctrl = params.ctrl;
    let vp = params.vp;
    let test_mem = params.test_mem;
    let io_gen = params.io_gen;

    // Generate the combined input buffer.
    let combined_input = [input_header.as_bytes(), var_header, input_reps.as_bytes()].concat();
    let output_len = size_of::<OutputT>() + output_reps.as_bytes().len();

    println!("fast: {}", params.fast);
    println!("size of input: {}", size_of::<InputT>());
    println!("var_header.len(): {}", var_header.len());
    println!("input_reps.len(): {}", input_reps.len());
    println!("size of output: {}", size_of::<OutputT>());
    println!("output_reps.len(): {}", output_reps.len());

    // If the caller does not provide an input control override, determine the appropriate call
    // code based on the types specified.
    // The only expected use case the override is to test the hypercall parser's  ability to
    // 1. Detect simple/rep calls that have an incorrect specification of rep count and
    // start index (count and index must be 0 for simple calls, and the count must be non-zero
    // with a start index less than the count for rep calls).
    // 2. Specifying variable input for non-variable calls and vice-versa.
    let input_control = params.input_control.unwrap_or_else(|| {
        let call_code = if !input_reps.is_empty() || !output_reps.is_empty() {
            if !var_header.is_empty() {
                TestHypercallCode::CallVariableRep
            } else if output_reps.is_empty() {
                TestHypercallCode::CallRepNoOutput
            } else {
                assert!(size_of::<OutputT>() == 0);
                TestHypercallCode::CallRep
            }
        } else {
            if !var_header.is_empty() {
                if size_of::<OutputT>() == 0 {
                    TestHypercallCode::CallVariableNoOutput
                } else {
                    TestHypercallCode::CallVariable
                }
            } else if size_of::<InputT>() != 0 {
                if size_of::<OutputT>() == 0 {
                    TestHypercallCode::CallSimpleNoOutput
                } else {
                    TestHypercallCode::CallSimple
                }
            } else {
                TestHypercallCode::CallNull
            }
        };

        println!("Selected call code: {:?}", call_code);

        // The caller should not map guest memory at the input/output GPA for a hypercall
        // without input/output. This allows catching stray accesses. This is only enforced when
        // the input control override is not present.
        assert!(!combined_input.is_empty() || !test_mem.input_allocated);
        assert!(output_len != 0 || !test_mem.output_allocated);

        Control::new()
            .with_code(call_code.0)
            .with_fast(params.fast)
            .with_rep_start(ctrl.reps.unwrap_or((0, 0)).0)
            .with_rep_count(input_reps.len())
            .with_variable_header_size(var_header.len() / 8)
    });

    println!("input control: {:?}", input_control);
    println!("TestController: {:?}", ctrl);

    let mut handler = TestHypercallHandler::new(ctrl, vp);
    let (mut io, input_register_pairs) = if !params.fast {
        // Prepare the input memory. Copy only as much data as can fit in the input buffer,
        // since the test may explicitly be trying to test straddling.
        let len = combined_input.len().min(PAGE_SIZE - params.in_offset);
        let input_buffer = &mut test_mem.internal_buffers[TestMemory::IN_INDEX]
            .0
            .as_mut_bytes()[params.in_offset..params.in_offset + len];
        input_buffer.copy_from_slice(&combined_input[..len]);

        // Write the input to guest memory.
        test_mem.buffers_to_gm();
        let mut io = io_gen(&mut handler);

        // Set the input and output GPAs. Even if the hypercall has no output if the
        // hypercall parser tries to write to the output page, it will (probably) access the
        // unallocated guard pages and fail.
        io.set_input_gpa((TestMemory::INPUT_BASE + params.in_offset) as u64);
        io.set_output_gpa((TestMemory::OUTPUT_BASE + params.out_offset) as u64);
        (io, None)
    } else {
        let mut io = io_gen(&mut handler);
        // Prepare the input registers. Copy only as much data as can fit in the fast registers
        // since the test may explicitly be trying to test straddling.
        let pair_count = io
            .fast_register_pair_count()
            .min(combined_input.len().div_ceil(16));

        if pair_count != 0 {
            let mut input_buffer = vec![[0u64; 2]; pair_count];
            input_buffer.as_mut_bytes()[..combined_input.len()]
                .copy_from_slice(combined_input.as_bytes());

            io.set_fast_input(&input_buffer[..pair_count]);
        }
        (io, Some(pair_count))
    };

    println!("Hypercall ABI details => {}", io.get_name());

    // Set the control register.
    io.set_control(input_control.into());

    // Clear the list of modified registers before the hypercall is dispatched. Any changes to
    // the mask now are due to the hypercall handlers.
    io.clear_modified_mask();

    // If the processor ABI advances the IP automatically on a hypercall, then simulate that here.
    io.auto_advance_ip();

    // Dispatch the hypercall.
    TestHypercallHandler::DISPATCHER.dispatch(&test_mem.gm, io);

    let is_timeout = ctrl.test_result.is_timeout();
    if is_timeout {
        assert_eq!(vp.ip, TestVp::INITIAL_IP);
    } else {
        assert_ne!(vp.ip, TestVp::INITIAL_IP);
    }

    {
        // Verify the expected return code.
        let mut handler = TestHypercallHandler::new(ctrl, vp);
        let mut io = io_gen(&mut handler);
        let result = HypercallOutput::from(io.get_result());
        let control = Control::from(io.control());
        let call_status = result.call_status();

        // Copy the output back. Note that in the case of errors the hypercall parser may not have
        // actually modified the output buffers/registers, and it is the responsibility of the test
        // to verify whether the correct operations were performed.
        // If the test expects the return status to be either HvError::InvalidHypercallInput or
        // HvError::InvalidAlignment, then it should explicitly pass in a guest memory object that
        // does not allow access to any of the input or output pages. That way the test can verify
        // that in cases where this routine does not modify the output, the hypercall parser does
        // not do so either.
        if is_timeout
            || (call_status != Err(HvError::InvalidHypercallInput).into()
                && call_status != Err(HvError::InvalidAlignment).into())
        {
            let mut output_buffer;
            let (hdr, reps) = if !params.fast {
                test_mem.gm_to_buffers();

                let output_buffer = &mut test_mem.internal_buffers[TestMemory::OUT_INDEX]
                    .0
                    .as_mut_bytes()[params.out_offset..params.out_offset + output_len];

                output_buffer.as_bytes().split_at(size_of::<OutputT>())
            } else {
                output_buffer = vec![[0u64; 2]; output_len.div_ceil(16)];
                io.get_fast_output(input_register_pairs.unwrap(), &mut output_buffer);
                let output_buffer = &mut output_buffer.as_mut_bytes()[..output_len];

                output_buffer.as_bytes().split_at(size_of::<OutputT>())
            };

            output.as_mut_bytes().copy_from_slice(hdr);
            output_reps.as_mut_bytes().copy_from_slice(reps);
        }

        (result, control)
    }
}

// &[()] is both the type of a slice of units, and an instance of a sliceof that type containing a
// single element. This routine allows specifying an empty slice when a test has no rep type.
fn no_reps() -> Vec<()> {
    Vec::new()
}

// Invoke a vtl switch hypercall.
fn invoke_vtl_switch_hypercall(params: InvokerParams<'_>, vtl_input: u64) {
    assert_eq!(params.in_offset, 0);
    assert_eq!(params.out_offset, 0);

    // If the caller overrides the input control, the correct value of fast should be specified in
    // the override.
    assert!(
        params
            .input_control
            .unwrap_or(Control::new().with_fast(params.fast))
            .fast()
            == params.fast
    );

    let ctrl = params.ctrl;
    let vp = params.vp;
    let test_mem = params.test_mem;
    let io_gen = params.io_gen;

    // If the caller does not provide an input control override, determine the appropriate call
    // code based on the types specified.
    // The only expected use case the override is to test the hypercall parser's  ability to
    // 1. Detect simple/rep calls that have an incorrect specification of rep count and
    // start index (count and index must be 0 for simple calls, and the count must be non-zero
    // with a start index less than the count for rep calls).
    // 2. Specifying variable input for non-variable calls and vice-versa.
    let input_control = params.input_control.unwrap_or_else(|| {
        let call_code = TestHypercallCode::CallVtl;
        Control::new().with_code(call_code.0).with_fast(params.fast)
    });

    let mut handler = TestHypercallHandler::new(ctrl, vp);
    let mut io = io_gen(&mut handler);
    io.set_vtl_input(vtl_input);

    println!("Hypercall ABI details => {}", io.get_name());

    // Set the control register.
    io.set_control(input_control.into());

    // Clear the list of modified registers before the hypercall is dispatched. Any changes to
    // the mask now are due to the hypercall handlers.
    io.clear_modified_mask();

    // Simulate the instruction pointer advance for ABIs that do so automatically.
    io.auto_advance_ip();

    // Dispatch the hypercall.
    TestHypercallHandler::DISPATCHER.dispatch(&test_mem.gm, io);
}

// Invoke a simple hypercall with no input or output.
fn hypercall_null(test_params: TestParams) {
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result);

    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(false, false), // No input or output,
            &test_params,
        ),
        &(),
        &[],
        no_reps().as_slice(),
        &mut (),
        no_reps().as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    assert_eq!(modified_mask, 0);
}

// Invoke a simple hypercall with no output.
fn hypercall_simple_no_output(test_params: TestParams) {
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result);

    let input_header = TestController::generate_test_input::<TestInput>();
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), false), // No output
            &TestParams {
                in_offset: test_params
                    .in_offset
                    .or(Some(PAGE_SIZE - size_of_val(&input_header))),
                ..test_params
            },
        ),
        &input_header,
        &[],
        no_reps().as_slice(),
        &mut (),
        no_reps().as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    assert_eq!(modified_mask, 0);
}

// Invoke a simple hypercall with input and output.
fn hypercall_simple(test_params: TestParams) {
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result);

    let input_header = TestController::generate_test_input::<TestInput>();
    let mut output = TestOutput::new_zeroed();
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), test_params.map_output()),
            &TestParams {
                in_offset: test_params
                    .in_offset
                    .or(Some(PAGE_SIZE - size_of_val(&input_header))),
                out_offset: test_params
                    .out_offset
                    .or(Some(PAGE_SIZE - size_of_val(&output))),
                ..test_params
            },
        ),
        &input_header,
        &[],
        no_reps().as_slice(),
        &mut output,
        no_reps().as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    let expected_output_size = if result.call_status().is_ok() {
        assert_eq!(
            output.as_bytes(),
            TestController::generate_test_output::<TestOutput>().as_bytes()
        );

        size_of_val(&output)
    } else {
        0
    };

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    let target_regpairs = if test_params.fast {
        expected_output_size.div_ceil(16)
    } else {
        0
    };
    assert_eq!(
        modified_mask.count_ones(),
        u32::try_from(target_regpairs * 2).unwrap()
    );
}

// Invoke a rep hypercall with no output.
fn hypercall_rep_no_output(test_params: TestParams) {
    let rep_count = test_params.rep_count.unwrap();
    let rep_start = test_params.rep_start.unwrap_or(0);
    let mut vp = TestVp::default();
    let mut ctrl = TestController::new(test_params.test_result).with_reps(rep_start, rep_count);

    let input_header = TestController::generate_test_input::<TestInput>();
    let input_reps = TestController::generate_input_reps::<u64>(rep_count);
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), false), // No output
            &TestParams {
                in_offset: test_params.in_offset.or(Some(
                    PAGE_SIZE - size_of_val(&input_header) - size_of_val(&input_reps[..]),
                )),
                ..test_params
            },
        ),
        &input_header,
        &[],
        input_reps.as_slice(),
        &mut (),
        no_reps().as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    assert_eq!(modified_mask, 0);
}

// Invoke a rep hypercall with input and output.
fn hypercall_rep(test_params: TestParams) {
    let rep_count = test_params.rep_count.unwrap();
    let rep_start = test_params.rep_start.unwrap_or(0);
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result).with_reps(rep_start, rep_count);

    let input_header = TestController::generate_test_input::<TestInput>();
    let input_reps = TestController::generate_input_reps::<u64>(rep_count);
    let mut output_reps = vec![BACK_PATTERN; rep_count];
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), test_params.map_output()),
            &TestParams {
                in_offset: test_params.in_offset.or(Some(
                    PAGE_SIZE - size_of_val(&input_header) - size_of_val(&input_reps[..]),
                )),
                out_offset: test_params
                    .out_offset
                    .or(Some(PAGE_SIZE - size_of_val(&output_reps[..]))),
                ..test_params
            },
        ),
        &input_header,
        &[],
        input_reps.as_slice(),
        &mut (),
        output_reps.as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    assert_eq!(
        output_reps[..rep_start].as_bytes(),
        vec![BACK_PATTERN; rep_start].as_bytes()
    );
    assert_eq!(
        output_reps[rep_start..test_params.test_result.expected_elements_processed()].as_bytes(),
        TestController::generate_output_reps::<u64>(
            test_params.test_result.expected_elements_processed()
        )[rep_start..]
            .as_bytes()
    );

    let elements_processed = test_params.test_result.expected_elements_processed();
    let elements_processed = if test_params.fast
        && !elements_processed.is_multiple_of(2)
        && elements_processed < rep_count
    {
        // Since only 16 byte writes are supported, the top 8 bytes are 0s.
        assert_eq!(output_reps[elements_processed], 0);
        elements_processed + 1
    } else {
        elements_processed
    };

    assert_eq!(
        output_reps[elements_processed..].as_bytes(),
        vec![BACK_PATTERN; rep_count - elements_processed].as_bytes()
    );

    {
        let mut expected_output_size = output_reps
            [rep_start..test_params.test_result.expected_elements_processed()]
            .as_bytes()
            .len();

        if !(rep_start * size_of::<u64>()).is_multiple_of(16) {
            expected_output_size += (rep_start * size_of::<u64>()) % 16;
        }

        if !(test_params.test_result.expected_elements_processed() * size_of::<u64>())
            .is_multiple_of(16)
        {
            expected_output_size += 16
                - ((test_params.test_result.expected_elements_processed() * size_of::<u64>()) % 16);
        }

        let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
        let io = (test_params.io_builder())(&mut handler);
        let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
        let target_regpairs = if test_params.fast {
            expected_output_size.div_ceil(16)
        } else {
            0
        };
        assert_eq!(
            modified_mask.count_ones(),
            u32::try_from(target_regpairs * 2).unwrap()
        );
    }
}

// Invoke a simple hypercall with a variable header and no output.
fn hypercall_variable_no_output(test_params: TestParams) {
    let var_size = test_params.var_size.unwrap();
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result).with_var_size(var_size);

    let input_header = TestController::generate_test_input::<TestInput>();
    let var_header = TestController::generate_var_header(var_size);
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), false), // No output
            &TestParams {
                in_offset: test_params.in_offset.or(Some(
                    PAGE_SIZE - size_of_val(&input_header) - var_header.len(),
                )),
                ..test_params
            },
        ),
        &input_header,
        &var_header,
        no_reps().as_slice(),
        &mut (),
        no_reps().as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    assert_eq!(modified_mask, 0);
}

// Invoke a simple hypercall with a variable header and input and output.
fn hypercall_variable(test_params: TestParams) {
    let var_size = test_params.var_size.unwrap();
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result).with_var_size(var_size);

    let input_header = TestController::generate_test_input::<TestInput>();
    let var_header = TestController::generate_var_header(var_size);
    let mut output = TestOutput::new_zeroed();
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), test_params.map_output()),
            &TestParams {
                in_offset: test_params.in_offset.or(Some(
                    PAGE_SIZE - size_of_val(&input_header) - var_header.len(),
                )),
                out_offset: test_params
                    .out_offset
                    .or(Some(PAGE_SIZE - size_of_val(&output))),
                ..test_params
            },
        ),
        &input_header,
        &var_header,
        no_reps().as_slice(),
        &mut output,
        no_reps().as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    let expected_output_size = if result.call_status().is_ok() {
        assert_eq!(
            output.as_bytes(),
            TestController::generate_test_output::<TestOutput>().as_bytes()
        );
        size_of_val(&output)
    } else {
        0
    };

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    let target_regpairs = if test_params.fast {
        expected_output_size.div_ceil(16)
    } else {
        0
    };
    assert_eq!(
        modified_mask.count_ones(),
        u32::try_from(target_regpairs * 2).unwrap()
    );
}

// Invoke a variable rep hypercall with input and output.
fn hypercall_variable_rep(test_params: TestParams) {
    let rep_count = test_params.rep_count.unwrap();
    let rep_start = test_params.rep_start.unwrap_or(0);
    let var_size = test_params.var_size.unwrap();
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result)
        .with_reps(rep_start, rep_count)
        .with_var_size(var_size);

    let input_header = TestController::generate_test_input::<TestInput>();
    let var_header = TestController::generate_var_header(var_size);
    let input_reps = TestController::generate_input_reps::<u64>(rep_count);
    let mut output_reps = vec![BACK_PATTERN; rep_count];
    let (result, control) = invoke_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(test_params.map_input(), test_params.map_output()),
            &TestParams {
                in_offset: test_params.in_offset.or(Some(
                    PAGE_SIZE
                        - size_of_val(&input_header)
                        - var_header.len()
                        - size_of_val(&input_reps[..]),
                )),
                out_offset: test_params
                    .out_offset
                    .or(Some(PAGE_SIZE - size_of_val(&output_reps[..]))),
                ..test_params
            },
        ),
        &input_header,
        &var_header,
        input_reps.as_slice(),
        &mut (),
        output_reps.as_mut_slice(),
    );

    check_test_result(&test_params, result, control);

    assert_eq!(
        output_reps[..rep_start].as_bytes(),
        vec![BACK_PATTERN; rep_start].as_bytes()
    );
    assert_eq!(
        output_reps[rep_start..test_params.test_result.expected_elements_processed()].as_bytes(),
        TestController::generate_output_reps::<u64>(
            test_params.test_result.expected_elements_processed()
        )[rep_start..]
            .as_bytes()
    );

    let elements_processed = test_params.test_result.expected_elements_processed();
    let elements_processed = if test_params.fast
        && !elements_processed.is_multiple_of(2)
        && elements_processed < rep_count
    {
        // Since only 16 byte writes are supported, the top 8 bytes are 0s.
        assert_eq!(output_reps[elements_processed], 0);
        elements_processed + 1
    } else {
        elements_processed
    };

    assert_eq!(
        output_reps[elements_processed..].as_bytes(),
        vec![BACK_PATTERN; rep_count - elements_processed].as_bytes()
    );

    let mut expected_output_size = output_reps
        [rep_start..test_params.test_result.expected_elements_processed()]
        .as_bytes()
        .len();

    if !(rep_start * size_of::<u64>()).is_multiple_of(16) {
        expected_output_size += (rep_start * size_of::<u64>()) % 16;
    }

    if !(test_params.test_result.expected_elements_processed() * size_of::<u64>())
        .is_multiple_of(16)
    {
        expected_output_size +=
            16 - ((test_params.test_result.expected_elements_processed() * size_of::<u64>()) % 16);
    }

    let mut handler = TestHypercallHandler::new(&mut ctrl, &mut vp);
    let io = (test_params.io_builder())(&mut handler);
    let modified_mask = io.get_modified_mask() & !io.get_io_register_mask();
    let target_regpairs = if test_params.fast {
        expected_output_size.div_ceil(16)
    } else {
        0
    };
    assert_eq!(
        modified_mask.count_ones(),
        u32::try_from(target_regpairs * 2).unwrap()
    );
}

fn vtl_switch_hypercall(test_params: &TestParams, vtl_input: u64) {
    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result);

    invoke_vtl_switch_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(false, false), // No memory input or output
            test_params,
        ),
        vtl_input,
    );

    assert_eq!(ctrl.vtl_controller.invoked, true);
}

fn vtl_return(test_params: &TestParams, vtl_input: u64, lower_vtl_enabled: bool) {
    assert_eq!(
        test_params.input_control.unwrap().code(),
        hvdef::HypercallCode::HvCallVtlReturn.0
    );

    let mut vp = Default::default();
    let mut ctrl = TestController::new(test_params.test_result);
    ctrl.vtl_controller.lower_vtl_enabled = lower_vtl_enabled;

    invoke_vtl_switch_hypercall(
        InvokerParams::new(
            &mut ctrl,
            &mut vp,
            &mut TestMemory::new(false, false), // No memory input or output
            test_params,
        ),
        vtl_input,
    );

    let test_success = matches!(test_params.test_result, TestResult::Vtl(true));
    assert_eq!(ctrl.vtl_controller.invoked, test_success);
    assert_eq!(vp.invalid_opcode, !test_success);
    assert_ne!(vp.ip == TestVp::INITIAL_IP, test_success);
}

#[test]
fn test_null() {
    let slow_params = TestParams::new(TestResult::Simple(SimpleResult::Success));
    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            hypercall_null(test_params.with_abi(abi));
        }
    }
}

#[test]
fn test_simple_no_output() {
    let slow_params = TestParams::new(TestResult::Simple(SimpleResult::Success));
    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            hypercall_simple_no_output(test_params.with_abi(abi));
        }
    }
}

#[test]
fn test_simple() {
    let slow_params = TestParams::new(TestResult::Simple(SimpleResult::Success));
    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                hypercall_simple(test_params.with_abi(abi));
            }
        }
    }
}

#[test]
fn test_rep_no_output() {
    const REP_COUNT: usize = 31;
    let slow_params =
        TestParams::new(TestResult::Rep(RepResult::Success(REP_COUNT))).with_rep_count(REP_COUNT);

    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                let test_params = if test_params.fast {
                    let rep_count = max_test_fast_rep_count(&abi);
                    test_params
                        .with_rep_count(rep_count)
                        .with_result_reps(rep_count)
                } else {
                    test_params
                };

                for rep_start in [0, 1] {
                    hypercall_rep_no_output(test_params.with_abi(abi).with_rep_start(rep_start));
                }
            }
        }
    }
}

#[test]
fn test_rep() {
    const REP_COUNT: usize = 31;
    let slow_params =
        TestParams::new(TestResult::Rep(RepResult::Success(REP_COUNT))).with_rep_count(REP_COUNT);

    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                let test_params = if test_params.fast {
                    let rep_count = max_test_fast_rep_count(&abi);
                    test_params
                        .with_rep_count(rep_count)
                        .with_result_reps(rep_count)
                } else {
                    test_params
                };

                for rep_start in [0, 1] {
                    hypercall_rep(test_params.with_abi(abi).with_rep_start(rep_start));
                }
            }
        }
    }
}

#[test]
fn test_rep_fail() {
    const REP_COUNT: usize = 3;
    let slow_params = TestParams::new(TestResult::Rep(RepResult::Failure(
        HvError::AccessDenied,
        REP_COUNT - 1,
    )))
    .with_rep_count(REP_COUNT);

    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                let test_params = if test_params.fast {
                    let rep_count = max_test_fast_rep_count(&abi);
                    test_params
                        .with_rep_count(rep_count)
                        .with_result_reps(rep_count - 1)
                } else {
                    test_params
                };

                for rep_start in [0, 1] {
                    hypercall_rep(test_params.with_abi(abi).with_rep_start(rep_start));
                }
            }
        }
    }
}

#[test]
fn test_rep_timeout() {
    const REP_COUNT: usize = 3;
    let slow_params = TestParams::new(TestResult::Rep(RepResult::Failure(
        HvError::Timeout,
        REP_COUNT - 1,
    )))
    .with_rep_count(REP_COUNT);

    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                for rep_start in [0, 1] {
                    hypercall_rep(test_params.with_abi(abi).with_rep_start(rep_start));
                }
            }
        }
    }
}

#[test]
fn test_variable_no_output() {
    const VAR_SIZE: usize = 24;
    let slow_params =
        TestParams::new(TestResult::Simple(SimpleResult::Success)).with_var_size(VAR_SIZE);
    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                hypercall_variable_no_output(test_params.with_abi(abi));
            }
        }
    }
}

#[test]
fn test_variable() {
    const VAR_SIZE: usize = 24;
    let slow_params =
        TestParams::new(TestResult::Simple(SimpleResult::Success)).with_var_size(VAR_SIZE);
    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                hypercall_variable(test_params.with_abi(abi));
            }
        }
    }
}

#[test]
fn test_variable_rep() {
    const REP_COUNT: usize = 3;
    const VAR_SIZE: usize = 24;

    let slow_params = TestParams::new(TestResult::Rep(RepResult::Success(REP_COUNT)))
        .with_rep_count(REP_COUNT)
        .with_var_size(VAR_SIZE);

    let fast_params = TestParams {
        fast: true,
        ..slow_params
    };

    for &abi in TestHypercallAbi::LIST {
        for test_params in [slow_params, fast_params] {
            for rep_start in [0, 1] {
                if abi.extended_fast_hypercalls_ok() || !test_params.fast {
                    hypercall_variable_rep(test_params.with_abi(abi).with_rep_start(rep_start));
                }
            }
        }
    }
}

#[test]
fn test_simple_with_reps() {
    for &abi in TestHypercallAbi::LIST {
        for test_params in [
            // Force a simple call with reps.
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidHypercallInput,
            )))
            .with_fast(false)
            .with_rep_count(3)
            .with_input_control(
                Control::new()
                    .with_code(TestHypercallCode::CallSimple.0)
                    .with_rep_count(3),
            ),
            // Do the same with a non-zero rep start index.
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidHypercallInput,
            )))
            .with_fast(false)
            .with_rep_count(3)
            .with_input_control(
                Control::new()
                    .with_code(TestHypercallCode::CallSimple.0)
                    .with_rep_start(1),
            ),
        ] {
            hypercall_rep_no_output(test_params.with_abi(abi));
        }
    }
}

#[test]
fn test_rep_incorrect_rep_params() {
    for &abi in TestHypercallAbi::LIST {
        for test_params in [
            // Force the use of a rep call, but set the rep count to 0.
            TestParams::new(TestResult::Rep(RepResult::Failure(
                HvError::InvalidHypercallInput,
                0,
            )))
            .with_rep_count(3)
            .with_input_control(
                Control::new()
                    .with_code(TestHypercallCode::CallRepNoOutput.0)
                    .with_rep_count(0),
            ),
            // Set rep start index to be equal to the rep count.
            TestParams::new(TestResult::Rep(RepResult::Failure(
                HvError::InvalidHypercallInput,
                0,
            )))
            .with_rep_count(3)
            .with_input_control(
                Control::new()
                    .with_code(TestHypercallCode::CallRepNoOutput.0)
                    .with_rep_count(3)
                    .with_rep_start(3),
            ),
        ] {
            hypercall_rep(test_params.with_abi(abi));
        }
    }
}

#[test]
fn test_unimplemented() {
    for &abi in TestHypercallAbi::LIST {
        // Invoke the reserved hypercall code which represents an entry not in the dispatcher table.
        // The hypercall should fail with HvError::InvalidHypercallCode.
        hypercall_null(
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidHypercallCode,
            )))
            .with_abi(abi)
            .with_input_control(Control::new().with_code(TestHypercallCode::CallReserved.0)),
        );
    }
}

fn max_test_fast_rep_count(abi: &TestHypercallAbi) -> usize {
    // Fast hypercalls. Maximum allowed IO size is 128 bytes on ARM64 and 112 bytes on
    // x86_64. The header is 16 bytes, which leaves either 112 or 96 bytes for reps. Half
    // of that is for the input reps (8 bytes each in our tests) and half is for the output
    // reps. However output must be on a 16 byte alignment.
    let mut max_rep_count = (abi.max_fast_output_size() - size_of::<TestInput>()) / (8 + 8);
    if !max_rep_count.is_multiple_of(2) {
        max_rep_count -= 1;
    }

    max_rep_count
}

#[test]
fn test_alignment_and_straddling() {
    for &abi in TestHypercallAbi::LIST {
        // Slow hypercalls
        for test_params in [
            // Test the alignment of the input buffer.
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidAlignment,
            )))
            .with_in_offset(4),
            // Test the alignment of the output buffer.
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidAlignment,
            )))
            .with_out_offset(4),
            // Test the case where the input buffer straddles two pages.
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidHypercallInput,
            )))
            .with_in_offset(PAGE_SIZE - 8),
            // Test the case where the output buffer straddles two pages.
            TestParams::new(TestResult::Simple(SimpleResult::Failure(
                HvError::InvalidHypercallInput,
            )))
            .with_out_offset(PAGE_SIZE - 8),
        ] {
            hypercall_simple(test_params.with_abi(abi));
        }

        if abi.extended_fast_hypercalls_ok() {
            // Fast hypercalls. Maximum allowed IO size is 128 bytes on ARM64 and 112 bytes on
            // x86_64. The header is 16 bytes, which leaves either 112 or 96 bytes for reps. Half
            // of that is for the input reps (8 bytes each in our tests) and half is for the output
            // reps. However output must be on a 16 byte alignment.
            let max_rep_count = max_test_fast_rep_count(&abi);

            hypercall_rep(
                TestParams::new(TestResult::Rep(RepResult::Success(max_rep_count)))
                    .with_abi(abi)
                    .with_fast(true)
                    .with_rep_count(max_rep_count),
            );

            // Specify one more than allowed.
            hypercall_rep(
                TestParams::new(TestResult::Rep(RepResult::Failure(
                    HvError::InvalidHypercallInput,
                    0,
                )))
                .with_abi(abi)
                .with_fast(true)
                .with_rep_count(max_rep_count + 1),
            );
        }
    }
}

#[test]
fn test_vtl() {
    vtl_switch_hypercall(
        &TestParams::new(TestResult::Vtl(true)).with_abi(TestHypercallAbi::X64 { is_64bit: true }),
        0,
    );
}

#[test]
fn test_vtl_return() {
    // Restrictions on the ability to implement VtlSwitchOps reasonably means that this test is only
    // supported with the X64 ABI. If another ABI is desired, the test will need to be updated to
    // store the ABI and explicitly construct the appropriate HypercallIo type. The IO builder will
    // not work due to lifetime concerns.
    for (test_params, input, lower_vtl_enabled) in [
        // Lower VTL enabled, valid input - should succeed.
        (
            TestParams::new(TestResult::Vtl(true))
                .with_abi(TestHypercallAbi::X64 { is_64bit: true })
                .with_input_control(
                    Control::new().with_code(hvdef::HypercallCode::HvCallVtlReturn.0),
                ),
            0,
            true,
        ),
        // Lower VTL enabled, invalid input - should fail.
        (
            TestParams::new(TestResult::Vtl(false))
                .with_abi(TestHypercallAbi::X64 { is_64bit: true })
                .with_input_control(
                    Control::new().with_code(hvdef::HypercallCode::HvCallVtlReturn.0),
                ),
            2,
            true,
        ),
        // Lower VTL disabled, valid input - should fail.
        (
            TestParams::new(TestResult::Vtl(false))
                .with_abi(TestHypercallAbi::X64 { is_64bit: true })
                .with_input_control(
                    Control::new().with_code(hvdef::HypercallCode::HvCallVtlReturn.0),
                ),
            0,
            false,
        ),
        // Lower VTL enabled, valid input, invalid input control bits - should fail.
        (
            TestParams::new(TestResult::Vtl(false))
                .with_abi(TestHypercallAbi::X64 { is_64bit: true })
                .with_input_control(
                    Control::new()
                        .with_code(hvdef::HypercallCode::HvCallVtlReturn.0)
                        .with_fast(true), // Invalid bit for vtl switches.
                )
                .with_fast(true),
            0,
            true,
        ),
    ] {
        vtl_return(&test_params, input, lower_vtl_enabled);
    }
}
