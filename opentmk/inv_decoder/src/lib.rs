// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Decodes and executes syzkaller programs against a caller-provided memory map.

#![no_std]

#[macro_use]
extern crate alloc;

mod atomicrefqueue;
mod safememory;
mod wire;

pub use safememory::SafeMemoryMap;

use spin::Mutex;

use core::{
    marker::PhantomData,
    ops,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use alloc::{string::String, sync::Arc, vec::Vec};
use anyhow::{Context, Result, bail};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::atomicrefqueue::AtomicRefQueue;

const K_MAX_COMMANDS: usize = 1000;

/// Required size, in bytes, of the syzkaller executor memory region.
pub const EXEC_INPUT_REQ_SIZE: usize = 0x1000000;

/// Supported (min) input size (kMaxInput in executor.cc).
pub const SUPPORTED_INPUT_SIZE: usize = 8 << 20;

/// Max supported args
pub const MAX_ARGS: usize = 30;

/// All syzkaller pointers are offsets from this presumed base address
pub const ADDR_SYZ_BEGIN: u64 = 0x20000000;

/// Call failed
const _SYZKALLER_CALL_END_FAILED: u64 = 3;

/// Results produced by executing the calls in a syzkaller program.
pub type TestcaseResults = [ResT; K_MAX_COMMANDS];

/// Callback used to execute a decoded syzkaller input.
pub type Executor<'f> =
    dyn Fn(&DecodedProgram<'_, '_>, InputCase) -> InputResult + 'f + Send + Sync;

/// Instructions for a decoded syzkaller program alongside the required
/// components to execute the program (e.g. the exec function and the results array).
///
/// 'm - lifetime of the memory map instance
/// 'f - lifetime of the executor function
pub struct DecodedProgram<'m, 'f> {
    instr_vec: Arc<AtomicRefQueue<InstrEntry>>,
    /// Memory-layout that holds the address layout of the syzkaller program
    mem: Arc<Mutex<dyn SafeMemoryMap + 'm>>,
    /// The executor function
    exec: Arc<Executor<'f>>,
    /// Holds the results for each call. Due to syzkaller internals, this only
    /// tracks results for calls that have direct return values (i.e. calls that are assigned
    /// a copyout index). Calls that are not assigned a copyout index are tracked in the custom_results)
    results: Arc<TestcaseResults>,
    /// Holds the results for calls that are not assigned a copyout index (i.e. calls that do not have
    /// direct return values)
    custom_results: Arc<TestcaseResults>,
}

/// Implementation for SafeMemoryMap. Note that because the memory map instance owned here
/// is behind a [`spin::Mutex`], so we can do memory operations without requiring
/// `&mut DecodedProgram` (we need this loosening of restrictions internally, i.e. so that
/// the reentrancy fuzz loop could perform read/write operations while holding only a
/// shared global reference to it).
impl SafeMemoryMap for &DecodedProgram<'_, '_> {
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        self.mem.lock().partial_write_mem(base, val)
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        self.mem.lock().partial_read_mem(base, val)
    }
}

impl<'m, 'f> DecodedProgram<'m, 'f> {
    /// Creates a new [`DecodedProgram`] that decodes the syzkaller program from the provided input
    /// buffer and exec callback.
    fn new<
        M: SafeMemoryMap + 'm,
        F: Fn(&DecodedProgram<'_, '_>, InputCase) -> InputResult + Send + Sync + 'f,
    >(
        mem: M,
        syz_input_buffer: &[u8],
        exec: F,
    ) -> Result<Self> {
        let mut decoder =
            Decoder::new(syz_input_buffer).context("failed to instantiate decoder")?;
        let mut instr_vec_tmp: Vec<InstrEntry> = Vec::new();
        {
            let mut call = None;
            let mut instrout: Vec<InstrCopyOut> = Vec::new();
            while let Some(instr) = decoder.try_next().context("failed to decode instruction")? {
                match instr {
                    Instr::CopyOut(i) => {
                        // CopyOuts always follow a call and are tied to calls, so we collect them
                        // and then attach them to their associated call later
                        instrout.push(i);
                    }
                    Instr::CopyIn(i) => {
                        let entry = InstrEntry::CopyIn(i);
                        instr_vec_tmp.push(entry);
                    }
                    Instr::Call(i) => {
                        // Process any cached call + copyouts if present
                        if let Some(call_entry) = call.take() {
                            let entry = InstrEntry::Call(call_entry, instrout.clone());
                            instr_vec_tmp.push(entry);
                            // Clear the collected CopyOuts vector.
                            instrout.clear();
                        }
                        // Now that we've flushed any previously cached call + copyouts, we can work with the
                        // current instruction.

                        // Cache the call, it'll be processed after any following CopyOuts
                        call = Some(i);
                    }
                }
            }

            // If the last instruction was a Call it would be in our cached `call` variable unprocessed.
            // If the last instruction was a CopyOut, we wouldn't have processed the last cached `call` yet either.
            // We do this here.
            if let Some(call_entry) = call.take() {
                let entry = InstrEntry::Call(call_entry, instrout.clone());
                instr_vec_tmp.push(entry);
                // Clear the collected CopyOuts vector.
                instrout.clear();
            }
        }

        let results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        let custom_results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        Ok(Self {
            mem: Arc::new(Mutex::new(mem)),
            instr_vec: Arc::new(AtomicRefQueue::new(instr_vec_tmp)),
            exec: Arc::new(exec),
            results: results.clone(),
            custom_results: custom_results.clone(),
        })
    }

    /// Checks if the instruction is a call, and if so whether it depends on the results of
    /// another call that has not executed yet
    fn is_instr_call_ready(&self, instr_entry: &InstrEntry) -> bool {
        // Get a reference to the global results array we're working with
        let results = self.results.as_ref();
        // Check if Instr is a call and if it depends on any unexecuted calls
        if let InstrEntry::Call(i, _) = &instr_entry {
            for arg in i.args.iter() {
                if let Arg::Result(a) = arg {
                    let r = &results[a.idx as usize];
                    if !r.executed.load(Ordering::SeqCst) {
                        // Found a dependent call that has not executed yet.
                        // Return false to indicate this call is not ready to be executed.
                        return false;
                    }
                }
            }
        }
        // Either Instr was not a call, or it was a call and all dependent calls have executed.
        // Return true to indicate this Instr is ready to be executed.
        true
    }

    /// Check if the provided call was executed and returned success, based on the contents
    /// of the provided results array.
    fn was_call_successful(&self, call: &InstrCall) -> bool {
        let (copyout_index, results) =
            if call.wire.copyout_index != wire::COPYOUT_INDEX_INVALID as u64 {
                (call.wire.copyout_index as usize, &self.results)
            } else {
                (
                    call.custom_copyout_index
                        .expect("copyout index was invalid, but no custom copyout index was set"),
                    &self.custom_results,
                )
            };

        if copyout_index >= K_MAX_COMMANDS {
            panic!(
                "copyout_call_results: result idx {:#x} overflows/underflows K_MAX_COMMANDS",
                copyout_index
            );
        }

        // Get the result value from the results array.
        let r = &results[copyout_index];

        r.was_successful()
    }

    /// Continues execution of the instructions contains in our own instruction vector
    pub fn continue_execution(&self) -> Result<()> {
        // SAFETY: This should always be sound because we cannot reach this point without having a valid
        // execution function.
        // We can _only_ take an immutable borrow here because this function is reentrant and we may have
        // other references to the execution function in higher stack frames.
        let instr_vec = self.instr_vec.clone();
        while let Some(entry) = instr_vec.pop_ref_conditional(|ent| self.is_instr_call_ready(ent)) {
            // Execute all the instructions, regardless of their type
            let instr_target = entry.get_inner_instr();
            self.exec_single(instr_target)
                .context("failed to execute instruction")?;

            // If the instr was a Call, it may have copyouts to execute.
            if let InstrEntry::Call(instr_call, copyouts) = entry {
                // Only execute copyouts if the call itself was successful, otherwise the values being
                // copied out may be invalid.
                if self.was_call_successful(instr_call) {
                    for copyout_entry in copyouts.iter() {
                        self.exec_single(Instr::CopyOut(*copyout_entry))
                            .context("failed to execute instruction")?;
                    }
                }
            } // No copyouts to process if the instruction was not a call
        }

        Ok(())
    }

    /// Executes the instructions in the provided instruction vector.
    fn exec_instrs(&self) -> Result<()> {
        let instr_vec = self.instr_vec.clone();
        while let Some(entry) = instr_vec.pop_ref_conditional(|ent| self.is_instr_call_ready(ent)) {
            // Execute all the instructions, regardless of their type
            self.exec_single(entry.get_inner_instr())
                .context("failed to execute instruction")?;

            // If the instr was a Call, it may have copyouts to execute.
            if let InstrEntry::Call(instr_call, copyouts) = entry {
                // Only execute copyouts if the call itself was successful, otherwise the values being
                // copied out may be invalid.
                if self.was_call_successful(instr_call) {
                    for copyout_entry in copyouts.iter() {
                        self.exec_single(Instr::CopyOut(*copyout_entry))
                            .context("failed to execute instruction")?;
                    }
                }
            } // No copyouts to process if the instruction was not a call
        }

        Ok(())
    }

    /// Executes the provided instruction.
    /// If the instruction is a call, the provided exec function is called with the provided input case
    /// and the results are stored in the results array if the call has a valid copyout index.
    fn exec_single(&self, instr: Instr) -> Result<()> {
        /// The number of bytes to offset all data operations by.
        const COPYIN_OFFSET: u64 = 0;

        let mut mem = self.mem.lock();
        match instr {
            Instr::CopyIn(i) => match i.arg {
                Arg::Const(a) => {
                    let size = a.meta & 0xff;
                    let bf = (a.meta >> 8) & 0xff;
                    let bf_off = (a.meta >> 16) & 0xff;
                    let bf_len = (a.meta >> 24) & 0xff;
                    let val = a.val + ((a.meta >> 32) * i.rpid);

                    copyin(
                        &mut *mem,
                        i.wire.addr + COPYIN_OFFSET,
                        val,
                        size,
                        bf,
                        bf_off,
                        bf_len,
                    );
                }
                Arg::Result(a) => {
                    let size = a.meta & 0xff;
                    let bf = (a.meta >> 8) & 0xff;

                    let r = &self.results[a.idx as usize];
                    let val = if r.was_successful() {
                        let mut v = r.val.load(Ordering::SeqCst);
                        v = v.checked_div(a.op_div).unwrap_or(v);
                        v + a.op_add
                    } else {
                        a.arg
                    };

                    copyin(&mut *mem, i.wire.addr + COPYIN_OFFSET, val, size, bf, 0, 0);
                }
                Arg::Data((a, d)) => {
                    mem.write_mem(
                        (i.wire.addr + COPYIN_OFFSET) as usize,
                        &d.as_bytes()[..a.size as usize],
                    );
                }
            },

            Instr::CopyOut(i) => {
                let mut val = 0u64;
                copyout(&mut *mem, i.wire.addr, i.wire.size, &mut val);

                let r = &self.results[i.wire.index as usize];
                // Its assumed if we're executing a CopyOut, that the associated call was
                // successful. We should not have been passed a CopyOut instruction to execute if
                // the associated call was not successful.
                r.mark_executed(true, val);
            }

            Instr::Call(i) => {
                // Evaluate all input arguments.
                let mut args = [0u64; MAX_ARGS];

                for (n, arg) in i.args.iter().enumerate() {
                    match arg {
                        Arg::Const(a) => {
                            // Calculate the constant value and store it in the input case.
                            let val = a.val + ((a.meta >> 32) * i.rpid);

                            args[n] = val;
                        }
                        Arg::Result(a) => {
                            let r = &self.results[a.idx as usize];
                            let val = if !r.was_successful() {
                                // The dependent call that's expected to fill this result argument value has either
                                // not been executed or did not execute successfully, meaning the result value
                                // is invalid. We will skip this call
                                return Ok(());
                            } else {
                                let mut v = r.val.load(Ordering::SeqCst);
                                v = v.checked_div(a.op_div).unwrap_or(v);
                                v + a.op_add
                            };

                            args[n] = val;
                        }
                        Arg::Data(_) => panic!("data argument for function call!?"),
                    }
                }

                let input_struct = InputCase {
                    call_num: i.idx as u64,
                    args,
                    num_args: i.args.len() as u64,
                    _priv: PhantomData,
                };

                // Ensure that we don't keep a lock to the safe memory map
                // when we pass execution to the executor. That way we can
                // perform reentrancy as needed.
                drop(mem);

                // Call the provided exec function now that we've parsed an input.
                let exec_result = (self.exec)(self, input_struct);

                // If the call has a valid associated copyout index, we need to set the result in the results array.
                let (copyout_index, results) =
                    if i.wire.copyout_index != wire::COPYOUT_INDEX_INVALID as u64 {
                        (i.wire.copyout_index as usize, &self.results)
                    } else {
                        (
                            i.custom_copyout_index.expect(
                                "copyout index was invalid, but no custom copyout index was set",
                            ),
                            &self.custom_results,
                        )
                    };

                if copyout_index >= K_MAX_COMMANDS {
                    panic!(
                        "copyout_call_results: result idx {:#x} overflows/underflows K_MAX_COMMANDS",
                        copyout_index
                    );
                }
                let r = &results[copyout_index];

                r.mark_executed(exec_result.is_success, exec_result.code);
            }
        }

        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct InstrCopyIn {
    /// The on-the-wire instruction.
    wire: wire::InstrCopyIn,
    /// The PID of the program.
    rpid: u64,
    /// The data source.
    arg: Arg,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum Arg {
    Const(wire::ArgConst),
    Result(wire::ArgResult),
    Data((wire::ArgData, Vec<u64>)),
    // CSum(),
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
struct InstrCopyOut {
    /// The on-the-wire instruction.
    wire: wire::InstrCopyOut,
    /// The PID of the program.
    rpid: u64,
}

#[derive(Debug, PartialEq, Eq, Clone)]
struct InstrCall {
    /// The on-the-wire instruction.
    wire: wire::InstrCall,
    /// The PID of the program.
    rpid: u64,
    /// The index of the function to call.
    idx: i64,
    /// Custom copyout index, *only* set if the wire::InstrCall::copyout_index is COPYOUT_INDEX_INVALID.
    /// This allows us to track success/failure of a call that has not been provided a copyout index from syzkaller.
    /// In this case, these indexes index into our custom results array (not the default results array).
    custom_copyout_index: Option<usize>,
    /// The arguments.
    args: Vec<Arg>,
}

/// A decoded syzkaller instruction.
enum InstrEntry {
    /// A call instruction with a list of associated copyout instructions.
    Call(InstrCall, Vec<InstrCopyOut>),
    /// A copyin instruction.
    CopyIn(InstrCopyIn),
}

impl InstrEntry {
    fn get_inner_instr(&self) -> Instr {
        match self {
            InstrEntry::Call(i, _) => Instr::Call(i.clone()),
            InstrEntry::CopyIn(i) => Instr::CopyIn(i.clone()),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum Instr {
    /// Copy data into memory.
    CopyIn(InstrCopyIn),
    /// Copy data out of memory into the results array.
    /// These instructions usually follow a call instruction.
    CopyOut(InstrCopyOut),
    /// Call a system call.
    Call(InstrCall),
}

struct Decoder<'a> {
    hdr: wire::ProgramHeader,
    buf: &'a [u8],
    custom_results_counter: AtomicUsize,
}

impl<'a> Decoder<'a> {
    fn new(mut buf: &'a [u8]) -> Result<Self> {
        let hdr = read_struct::<wire::ProgramHeader>(&mut buf)
            .context("failed to read program header")?;

        if hdr.magic != 0xbadc0ffeebadface {
            bail!("bad execute request magic {:#?}", hdr.magic);
        } else if hdr.prog_size == 0 {
            bail!("prog size is 0");
        } else if hdr.prog_size
            > ((SUPPORTED_INPUT_SIZE as u64) - (size_of::<wire::ProgramHeader>() as u64))
        {
            bail!("input size too large! InputSz:{:#?}", hdr.prog_size);
        }

        if hdr.prog_size > buf.len() as u64 {
            bail!("syzkaller program is larger than input buffer");
        }

        Ok(Self {
            // Resize the buffer to the program size.
            buf: &buf[..hdr.prog_size as usize],
            hdr,
            custom_results_counter: AtomicUsize::new(0),
        })
    }

    /// Fetch and decode the next instruction. Returns `None` if we have reached EOF.
    pub fn try_next(&mut self) -> Result<Option<Instr>> {
        let n = read_inc_input(&mut self.buf).context("unexpected end of input")? as i64;
        match n {
            wire::INSTR_EOF => Ok(None),

            wire::INSTR_COPYIN => {
                let insn = read_struct::<wire::InstrCopyIn>(&mut self.buf)
                    .context("failed to read instruction")?;

                let arg = read_arg(&mut self.buf).context("failed to read argument")?;

                Ok(Some(Instr::CopyIn(InstrCopyIn {
                    rpid: self.hdr.pid,
                    wire: insn,
                    arg,
                })))
            }

            wire::INSTR_COPYOUT => {
                let insn = read_struct::<wire::InstrCopyOut>(&mut self.buf)
                    .context("failed to read instruction")?;

                Ok(Some(Instr::CopyOut(InstrCopyOut {
                    rpid: self.hdr.pid,
                    wire: insn,
                })))
            }

            c if c < 0 => {
                bail!("unknown instruction {n}");
            }

            _ => {
                let insn = read_struct::<wire::InstrCall>(&mut self.buf)
                    .context("failed to read instruction")?;

                let mut args = Vec::new();
                for _i in 0..insn.num_args {
                    args.push(read_arg(&mut self.buf).context("failed to read argument")?);
                }
                let custom_copyout_index =
                    if insn.copyout_index == wire::COPYOUT_INDEX_INVALID as u64 {
                        Some(self.custom_results_counter.fetch_add(1, Ordering::SeqCst))
                    } else {
                        None
                    };
                Ok(Some(Instr::Call(InstrCall {
                    rpid: self.hdr.pid,
                    idx: n,
                    wire: insn,
                    args,
                    custom_copyout_index,
                })))
            }
        }
    }
}

/// Represents a parsed syzkaller input.
pub struct InputCase {
    /// Syzkaller call number to execute.
    pub call_num: u64,
    /// argument array
    pub args: [u64; MAX_ARGS],
    /// Number of args set in `args`.
    pub num_args: u64,
    /// Prevent construction by our callers so we can ensure maximum SemVer flexibility.
    #[doc(hidden)]
    _priv: PhantomData<()>,
}

/// Result of an executed syzkaller input
#[derive(Default)]
pub struct InputResult {
    /// Value returned by the executed call.
    pub code: u64,
    /// Name of the executed call.
    pub name: String,
    /// Whether the call completed successfully.
    pub is_success: bool,
}

/// Thread-safe result storage for a single executed call.
pub struct ResT {
    executed: AtomicBool,
    val: AtomicU64,
    is_success: AtomicBool,
}

// Derive Clone for ResT by simply copying the atomic values.
impl Clone for ResT {
    fn clone(&self) -> Self {
        ResT {
            executed: AtomicBool::new(self.executed.load(Ordering::SeqCst)),
            val: AtomicU64::new(self.val.load(Ordering::SeqCst)),
            is_success: AtomicBool::new(self.is_success.load(Ordering::SeqCst)),
        }
    }
}

impl ResT {
    const fn new() -> Self {
        ResT {
            executed: AtomicBool::new(false),
            val: AtomicU64::new(0),
            is_success: AtomicBool::new(false),
        }
    }

    /// Returns whether the result entry has been executed and was successful
    pub fn was_successful(&self) -> bool {
        self.executed.load(Ordering::SeqCst) && self.is_success.load(Ordering::SeqCst)
    }

    /// Returns the contained value. The value is only returned if the result entry has been executed.
    pub fn value(&self) -> Option<u64> {
        if self.executed.load(Ordering::SeqCst) {
            Some(self.val.load(Ordering::SeqCst))
        } else {
            None
        }
    }

    // Mark the result as executed, whether it was successful, and sets the value
    fn mark_executed(&self, is_success: bool, val: u64) {
        // First, update the value.
        // This *must* occur before we set is_success/executed to ensure other threads don't attempt
        // to read the value until executed/is_success are also set.
        self.val.store(val, Ordering::SeqCst);

        self.executed.store(true, Ordering::SeqCst);
        self.is_success.store(is_success, Ordering::SeqCst);
    }
}

/// Read a 64-bit little-endian word from a slice and advance the slice's pointer.
///
/// This will panic if the slice pointed to by `input_data` is smaller than 8 bytes.
fn read_inc_input(input_data: &mut &[u8]) -> Option<u64> {
    let (s, t) = u64::read_from_prefix(input_data).ok()?;
    *input_data = t;
    Some(s)
}

/// Read a struct out of `input_data` and advance the slice's pointer.
fn read_struct<T: FromBytes>(input_data: &mut &[u8]) -> Option<T> {
    let (s, t) = T::read_from_prefix(input_data).ok()?;

    *input_data = t;
    Some(s)
}

/// Read out an argument from an input buffer.
fn read_arg(buf: &mut &[u8]) -> Result<Arg> {
    let typ = read_inc_input(buf).context("unexpected end of input")?;

    Ok(match typ {
        wire::ARG_CONST => {
            let arg = read_struct::<wire::ArgConst>(buf).context("failed to read argument")?;

            Arg::Const(arg)
        }
        wire::ARG_RESULT => {
            let arg = read_struct::<wire::ArgResult>(buf).context("failed to read argument")?;

            Arg::Result(arg)
        }
        wire::ARG_DATA => {
            let arg = read_struct::<wire::ArgData>(buf).context("failed to read argument")?;

            // Read out each data word.
            let cnt = arg.size.div_ceil(8);
            if cnt == 0 {
                panic!("data argument with size of zero!?");
            }

            let mut words = Vec::new();
            for _i in 0..cnt {
                words.push(read_inc_input(buf).context("unexpected end of input")?);
            }

            Arg::Data((arg, words))
        }
        // arg_csum
        0x3 => todo!(),
        // Catchall
        _ => bail!("unsupported argument type: {typ}"),
    })
}

fn store_by_bitmask<M, N>(mem: &mut M, addr: u64, val: N, bf_off: u64, bf_len: u64)
where
    M: SafeMemoryMap + ?Sized,
    N: FromBytes
        + IntoBytes
        + Immutable
        + Default
        + Copy
        + num_traits::Num
        + ops::Shl<u64, Output = N>
        + ops::Not<Output = N>
        + ops::BitOrAssign
        + ops::BitAndAssign
        + ops::BitAnd<Output = N>,
{
    if bf_off == 0 && bf_len == 0 {
        mem.write_mem(addr as usize, val.as_bytes())
    } else {
        let mut new_val = N::default();
        mem.read_mem(addr as usize, new_val.as_mut_bytes());

        // unset bitmask
        let mask = (N::one() << bf_len) - N::one();
        new_val &= !(mask << bf_off);

        // set val into bitmask
        new_val |= (val & mask) << bf_off;

        mem.write_mem(addr as usize, new_val.as_bytes())
    }
}

/// Copy a value into a buffer with the specified binary format (in `bf`).
fn copyin<M: SafeMemoryMap + ?Sized>(
    mem: &mut M,
    addr: u64,
    val: u64,
    size: u64,
    bf: u64,
    bf_off: u64,
    bf_len: u64,
) {
    if bf != 0 && (bf_off != 0 || bf_len != 0) {
        panic!("copyin: bitmask for string format invalid");
    }

    let tmp_str = match bf {
        // Case: binary_format_native
        0 => {
            match size {
                1 => store_by_bitmask(mem, addr, val as u8, bf_off, bf_len),
                2 => store_by_bitmask(mem, addr, val as u16, bf_off, bf_len),
                4 => store_by_bitmask(mem, addr, val as u32, bf_off, bf_len),
                8 => store_by_bitmask(mem, addr, val, bf_off, bf_len),
                _ => {
                    panic!("copyin: bad argument size {}", size);
                }
            }
            return;
        }

        // Case: binary_format_bigendian
        1 => panic!("unhandled: bigendian binary format"),

        // Case: binary_format_strdec
        2 => {
            // Converts 0xffffffffffffffff into 34343736`34343831
            // 35353930`37333730 00000000`35313631
            // TODO: verify endianness & correctness and re-assess implementation for perf
            assert!(size == 20);
            format!("{val:020}")
        }

        // Case: binary_format_strhex
        3 => {
            // Stores ascii of hex, e.g val 0xffffffffffffffff turns
            // into 0x66666666`66667830 66666666`66666666
            // TODO: verify endianness & correctness and re-assess implementation for perf
            assert!(size == 18);
            format!("{val:#018x}")
        }

        // Case: binary_format_stroct
        4 => {
            // turns 0xffffffffffffffff into 37373737`37373130
            // 37373737`37373737 00373737`37373737
            // TODO: verify endianness & correctness and re-assess implementation for perf
            assert!(size == 23);
            format!("{val:0023o}")
        }

        _ => {
            panic!("copyin: unknown binary format {}", bf);
        }
    };

    assert!(tmp_str.len() == size as usize);

    mem.write_mem(addr as usize, tmp_str.as_bytes())
}

fn copyout<M: SafeMemoryMap + ?Sized>(mem: &mut M, addr: u64, size: u64, res: &mut u64) {
    // NB: this code makes an assumption that we are working with LSB only architectures
    const _STATIC_ASSERT_IS_LSB: [u8; (1 - u16::from_le_bytes([1, 0])) as usize] = []; // if this errors we are compiling to an MSB arch

    let mut buf = [0; 8];
    let readlen = size.min(8) as usize;
    mem.read_mem(addr as usize, &mut buf[0..readlen]);
    match size {
        1 => (),
        2 => (),
        4 => (),
        8 => (),
        _ => panic!("copyout: bad argument size {:#x}", size),
    }

    *res = u64::from_le_bytes(buf);
}

/// Takes a syz_in buffer containing raw syzkaller data from TKO, parses
/// individual test cases from it into the provided addr buffer, and calls the
/// provided exec function with a single test case; then continues from the
/// beginning until all provided testcases have completed.
///
/// It is expected that addr_size is minimum 0x1000000 bytes (or 4mb).
/// syz_exec_mem must be at least [`EXEC_INPUT_REQ_SIZE`] in size.
/// syz_input_buffer must be at least [`SUPPORTED_INPUT_SIZE`] in size.
pub fn exec_testcases_safe<M: SafeMemoryMap, F>(
    syz_exec_mem: M,
    syz_input_buffer: &mut [u8],
    exec: F,
) -> Result<TestcaseResults>
where
    F: Fn(&DecodedProgram<'_, '_>, InputCase) -> InputResult + Send + Sync,
{
    let decoded = DecodedProgram::new(syz_exec_mem, syz_input_buffer, exec)?;
    decoded.exec_instrs()?;
    Ok(decoded.results.as_ref().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;

    #[test]
    fn test_decode() {
        const BUF: &[u8] = &[
            0xce, 0xfa, 0xad, 0xeb, 0xfe, 0x0f, 0xdc, 0xba, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xff, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xfb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];

        let mut decoder = Decoder::new(BUF).unwrap();
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(0),
                args: vec![],
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(1),
                args: vec![]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 0,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 2
                },
                custom_copyout_index: Some(2),
                args: vec![
                    Arg::Const(wire::ArgConst { meta: 4, val: 2047 }),
                    Arg::Const(wire::ArgConst {
                        meta: 4,
                        val: 0x1_00000001
                    })
                ]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(3),
                args: vec![]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(4),
                args: vec![]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 0,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 2
                },
                custom_copyout_index: Some(5),
                args: vec![
                    Arg::Const(wire::ArgConst { meta: 4, val: 5 }),
                    Arg::Const(wire::ArgConst { meta: 4, val: 4 })
                ]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(6),
                args: vec![]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(7),
                args: vec![]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 0,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 2
                },
                custom_copyout_index: Some(8),
                args: vec![
                    Arg::Const(wire::ArgConst { meta: 4, val: 8 }),
                    Arg::Const(wire::ArgConst {
                        meta: 4,
                        val: 0xFFFFFFFFFFFFFFFB
                    })
                ]
            }))
        );
        assert_eq!(
            decoder.try_next().unwrap(),
            Some(Instr::Call(InstrCall {
                idx: 1,
                rpid: 0,
                wire: wire::InstrCall {
                    copyout_index: !0,
                    num_args: 0
                },
                custom_copyout_index: Some(9),
                args: vec![]
            }))
        );
        assert_eq!(decoder.try_next().unwrap(), None);
    }

    #[test]
    fn test_copyin() {
        let mut mem: [u8; 8] = [0; 8];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0x1234567890abcdef;
        let size: u64 = 8;
        let bf: u64 = 0; // binary_format_native
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);

        let expected: [u8; 8] = [0xef, 0xcd, 0xab, 0x90, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(mem, expected);
    }

    #[test]
    fn test_copyin_with_bitmask() {
        let mut mem: [u8; 4] = [0; 4];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0x5;
        let size: u64 = 1;
        let bf: u64 = 0; // binary_format_native
        let bf_off: u64 = 2;
        let bf_len: u64 = 2;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);

        let expected: [u8; 4] = [0x4, 0x0, 0x0, 0x0];
        assert_eq!(mem, expected);
    }

    #[test]
    #[should_panic(expected = "copyin: bad argument size 3")]
    fn test_copyin_with_invalid_size() {
        let mut mem: [u8; 8] = [0; 8];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0x1234567890abcdef;
        let size: u64 = 3;
        let bf: u64 = 0; // binary_format_native
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);
    }

    #[test]
    #[should_panic(expected = "copyin: unknown binary format 5")]
    fn test_copyin_with_unknown_binary_format() {
        let mut mem: [u8; 8] = [0; 8];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0x1234567890abcdef;
        let size: u64 = 8;
        let bf: u64 = 5;
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);
    }

    #[test]
    fn test_copyin_format_strdec() {
        let mut mem: [u8; 20] = [0; 20];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0xffffffffffffffff;
        let size: u64 = 20;
        let bf: u64 = 2; // binary_format_strdec
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);

        // 0xffffffffffffffff -> 34343736`34343831 35353930`37333730 00000000`35313631 (ascii dec)
        let expected: [u8; 20] = [
            0x31, 0x38, 0x34, 0x34, 0x36, 0x37, 0x34, 0x34, 0x30, 0x37, 0x33, 0x37, 0x30, 0x39,
            0x35, 0x35, 0x31, 0x36, 0x31, 0x35,
        ];
        assert_eq!(mem, expected);
    }

    #[test]
    fn test_copyin_format_strhex() {
        let mut mem: [u8; 18] = [0; 18];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0xffffffffffffffff;
        let size: u64 = 18;
        let bf: u64 = 3; // binary_format_strhex
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);

        // 0xffffffffffffffff -> 0x66666666`66667830 66666666`66666666 (ascii hex)
        let expected: [u8; 18] = [
            0x30, 0x78, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
            0x66, 0x66, 0x66, 0x66,
        ];
        assert_eq!(mem, expected);
    }

    #[test]
    fn test_copyin_format_stroct() {
        let mut mem: [u8; 23] = [0; 23];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0xffffffffffffffff;
        let size: u64 = 23;
        let bf: u64 = 4; // binary_format_stroct
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len);

        // 0xffffffffffffffff -> 37373737`37373130 37373737`37373737 00373737`37373737 (ascii oct)
        let expected: [u8; 23] = [
            0x30, 0x31, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
        ];
        assert_eq!(mem, expected);
    }

    fn make_noop_prog_with_results(
        mem: Box<dyn SafeMemoryMap>,
        results: Arc<TestcaseResults>,
    ) -> DecodedProgram<'static, 'static> {
        let noop_exec: Arc<Executor<'static>> =
            Arc::new(|_: &DecodedProgram<'_, '_>, _: InputCase| InputResult::default());
        DecodedProgram {
            instr_vec: Arc::new(AtomicRefQueue::new(vec![])),
            mem: Arc::new(Mutex::new(mem)),
            exec: noop_exec,
            results,
            custom_results: Arc::new([const { ResT::new() }; K_MAX_COMMANDS]),
        }
    }

    // exec_single CopyIn Arg::Result path: result was successful, op_div and op_add are applied.
    // results[0] = 200, op_div = 4, op_add = 10 → (200 / 4) + 10 = 60
    #[test]
    fn test_exec_single_copyin_result_applies_op_div_and_op_add() {
        let results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        results[0].mark_executed(true, 200);

        let mem = Box::new([0u8; 8]);
        let addr = mem.as_ptr() as u64;
        let prog = make_noop_prog_with_results(mem, results);

        let instr = Instr::CopyIn(InstrCopyIn {
            wire: wire::InstrCopyIn { addr },
            rpid: 0,
            arg: Arg::Result(wire::ArgResult {
                meta: 4, // size=4, binary_format_native
                idx: 0,
                op_div: 4,
                op_add: 10,
                arg: 0xFFFFFFFF,
            }),
        });

        prog.exec_single(instr).unwrap();

        let mut buf = [0u8; 4];
        prog.mem.lock().read_mem(addr as usize, &mut buf);
        let written = u32::from_le_bytes(buf);
        assert_eq!(written, 60); // (200 / 4) + 10
    }

    // exec_single CopyIn Arg::Result path: op_div is 0 so division is skipped.
    // results[0] = 100, op_div = 0, op_add = 7 → 100 + 7 = 107
    #[test]
    fn test_exec_single_copyin_result_op_div_zero_skips_division() {
        let results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        results[0].mark_executed(true, 100);

        let mem = Box::new([0u8; 8]);
        let addr = mem.as_ptr() as u64;
        let prog = make_noop_prog_with_results(mem, results);

        let instr = Instr::CopyIn(InstrCopyIn {
            wire: wire::InstrCopyIn { addr },
            rpid: 0,
            arg: Arg::Result(wire::ArgResult {
                meta: 4,
                idx: 0,
                op_div: 0,
                op_add: 7,
                arg: 0xFFFFFFFF,
            }),
        });

        prog.exec_single(instr).unwrap();

        let mut buf = [0u8; 4];
        prog.mem.lock().read_mem(addr as usize, &mut buf);
        let written = u32::from_le_bytes(buf);
        assert_eq!(written, 107); // 100 + 7
    }

    // exec_single CopyIn Arg::Result path: result was NOT successful, falls back to ArgResult.arg default.
    // results[0] not executed, arg (default) = 42 → 42
    #[test]
    fn test_exec_single_copyin_result_uses_default_when_not_successful() {
        let results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        // results[0] left unexecuted

        let mem = Box::new([0u8; 8]);
        let addr = mem.as_ptr() as u64;
        let prog = make_noop_prog_with_results(mem, results);

        let instr = Instr::CopyIn(InstrCopyIn {
            wire: wire::InstrCopyIn { addr },
            rpid: 0,
            arg: Arg::Result(wire::ArgResult {
                meta: 4,
                idx: 0,
                op_div: 2,
                op_add: 5,
                arg: 42,
            }),
        });

        prog.exec_single(instr).unwrap();

        let mut buf = [0u8; 4];
        prog.mem.lock().read_mem(addr as usize, &mut buf);
        let written = u32::from_le_bytes(buf);
        assert_eq!(written, 42); // default, op_div/op_add not applied
    }

    // exec_single Call Arg::Result path: result was successful, op_div and op_add are applied to the call arg.
    // results[0] = 200, op_div = 4, op_add = 10 → exec receives (200 / 4) + 10 = 60
    #[test]
    fn test_exec_single_call_result_applies_op_div_and_op_add() {
        let results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        results[0].mark_executed(true, 200);

        let captured_arg = Arc::new(AtomicU64::new(0));
        let captured_clone = captured_arg.clone();
        let exec_fn: Arc<Executor<'static>> =
            Arc::new(move |_: &DecodedProgram<'_, '_>, input: InputCase| -> InputResult {
                captured_clone.store(input.args[0], Ordering::SeqCst);
                InputResult {
                    code: 0,
                    name: String::new(),
                    is_success: true,
                }
            });

        let mem: Box<dyn SafeMemoryMap> = Box::new([0u8; 8]);
        let prog = DecodedProgram {
            instr_vec: Arc::new(AtomicRefQueue::new(vec![])),
            mem: Arc::new(Mutex::new(mem)),
            exec: exec_fn,
            results,
            custom_results: Arc::new([const { ResT::new() }; K_MAX_COMMANDS]),
        };

        let instr = Instr::Call(InstrCall {
            wire: wire::InstrCall {
                copyout_index: wire::COPYOUT_INDEX_INVALID as u64,
                num_args: 1,
            },
            rpid: 0,
            idx: 0,
            custom_copyout_index: Some(0),
            args: vec![Arg::Result(wire::ArgResult {
                meta: 4,
                idx: 0,
                op_div: 4,
                op_add: 10,
                arg: 0xFFFFFFFF,
            })],
        });

        prog.exec_single(instr).unwrap();

        let val = captured_arg.load(Ordering::SeqCst);
        assert_eq!(val, 60); // (200 / 4) + 10
    }

    // exec_single Call Arg::Result path: result was NOT successful, the call is skipped entirely.
    // results[0] not executed → exec function should never be invoked.
    #[test]
    fn test_exec_single_call_result_skips_when_not_successful() {
        let results = Arc::new([const { ResT::new() }; K_MAX_COMMANDS]);
        // results[0] left unexecuted

        let was_called = Arc::new(AtomicBool::new(false));
        let was_called_clone = was_called.clone();
        let exec_fn: Arc<Executor<'static>> =
            Arc::new(move |_: &DecodedProgram<'_, '_>, _: InputCase| -> InputResult {
                was_called_clone.store(true, Ordering::SeqCst);
                InputResult::default()
            });

        let mem: Box<dyn SafeMemoryMap> = Box::new([0u8; 8]);
        let prog = DecodedProgram {
            instr_vec: Arc::new(AtomicRefQueue::new(vec![])),
            mem: Arc::new(Mutex::new(mem)),
            exec: exec_fn,
            results,
            custom_results: Arc::new([const { ResT::new() }; K_MAX_COMMANDS]),
        };

        let instr = Instr::Call(InstrCall {
            wire: wire::InstrCall {
                copyout_index: wire::COPYOUT_INDEX_INVALID as u64,
                num_args: 1,
            },
            rpid: 0,
            idx: 0,
            custom_copyout_index: Some(0),
            args: vec![Arg::Result(wire::ArgResult {
                meta: 4,
                idx: 0,
                op_div: 2,
                op_add: 5,
                arg: 99,
            })],
        });

        prog.exec_single(instr).unwrap();

        assert!(!was_called.load(Ordering::SeqCst));
    }
}
