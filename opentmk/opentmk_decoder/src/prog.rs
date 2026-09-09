use crate::K_MAX_COMMANDS;
use crate::MAX_ARGS;
use crate::decoder::Decoder;
use crate::decoder::DecoderError;
use crate::instr::Arg;
use crate::instr::Instr;
use crate::instr::InstrCall;
use crate::instr::InstrCopyOut;
use crate::instr::InstrEntry;
use crate::safememory::SafeMemoryMap;
use crate::wire;

use core::{
    marker::PhantomData,
    ops,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

use alloc::{collections::vec_deque::VecDeque, string::String, sync::Arc, vec::Vec};
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// Results produced by executing the calls in a syzkaller program.
pub type TestcaseResults = [ResT; K_MAX_COMMANDS];

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

/// Instructions for a decoded syzkaller program alongside the required
/// components to execute the program (e.g. the exec function and the results array).
///
/// 'm - lifetime of the memory map instance
/// 'f - lifetime of the executor function
pub(crate) struct DecodedProgram<M, F> {
    instr_vec: VecDeque<InstrEntry>,
    /// Memory-layout that holds the address layout of the syzkaller program
    mem: M,
    /// The executor function
    exec: F,
    /// Holds the results for each call. Due to syzkaller internals, this only
    /// tracks results for calls that have direct return values (i.e. calls that are assigned
    /// a copyout index). Calls that are not assigned a copyout index are tracked in the custom_results)
    pub(crate) results: Arc<TestcaseResults>,
    /// Holds the results for calls that are not assigned a copyout index (i.e. calls that do not have
    /// direct return values)
    custom_results: Arc<TestcaseResults>,
}

/// Implementation for SafeMemoryMap. Note that because the memory map instance owned here
/// is behind a [`spin::Mutex`], so we can do memory operations without requiring
/// `&mut DecodedProgram` (we need this loosening of restrictions internally, i.e. so that
/// the reentrancy fuzz loop could perform read/write operations while holding only a
/// shared global reference to it).
impl<M, F> SafeMemoryMap for DecodedProgram<M, F>
where
    M: SafeMemoryMap,
    F: Fn(&M, InputCase) -> InputResult + Send + Sync,
{
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        self.mem.partial_write_mem(base, val)
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        self.mem.partial_read_mem(base, val)
    }
}

impl<F> DecodedProgram<Vec<u8>, F>
where
    F: Fn(&mut Vec<u8>, InputCase) -> InputResult + Send + Sync,
{
    #[cfg(test)]
    fn test_new(exec: F, results: Arc<TestcaseResults>) -> Self {
        DecodedProgram {
            instr_vec: VecDeque::new(),
            mem: vec![0u8; 8],
            exec,
            results,
            custom_results: Arc::new([const { ResT::new() }; K_MAX_COMMANDS]),
        }
    }
}

impl<M, F> DecodedProgram<M, F>
where
    M: SafeMemoryMap,
    F: Fn(&mut M, InputCase) -> InputResult + Send + Sync,
{
    /// Creates a new [`DecodedProgram`] that decodes the syzkaller program from the provided input
    /// buffer and exec callback.
    pub(crate) fn new(mem: M, syz_input_buffer: &[u8], exec: F) -> Result<Self, DecoderError> {
        let mut decoder = Decoder::new(syz_input_buffer)?;
        let mut instr_vec: Vec<InstrEntry> = Vec::new();
        {
            let mut call = None;
            let mut instrout: Vec<InstrCopyOut> = Vec::new();
            while let Some(instr) = decoder.try_next()? {
                match instr {
                    Instr::CopyOut(i) => {
                        // CopyOuts always follow a call and are tied to calls, so we collect them
                        // and then attach them to their associated call later
                        instrout.push(i);
                    }
                    Instr::CopyIn(i) => {
                        let entry = InstrEntry::CopyIn(i);
                        instr_vec.push(entry);
                    }
                    Instr::Call(i) => {
                        // Process any cached call + copyouts if present
                        if let Some(call_entry) = call.take() {
                            let entry = InstrEntry::Call(call_entry, instrout.clone());
                            instr_vec.push(entry);
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
                instr_vec.push(entry);
                // Clear the collected CopyOuts vector.
                instrout.clear();
            }
        }

        Ok(Self {
            mem,
            instr_vec: instr_vec.into(),
            exec,
            results: Arc::new([const { ResT::new() }; K_MAX_COMMANDS]),
            custom_results: Arc::new([const { ResT::new() }; K_MAX_COMMANDS]),
        })
    }

    /// Check if the provided call was executed and returned success, based on the contents
    /// of the provided results array.
    fn was_call_successful(&self, call: &InstrCall) -> Result<bool, DecoderError> {
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
            return Err(DecoderError::OverflowOutIndex(copyout_index));
        }

        // Get the result value from the results array.
        let r = &results[copyout_index];

        Ok(r.was_successful())
    }

    pub(crate) fn exec_instrs(&mut self) -> Result<(), DecoderError> {
        while let Some(entry) = self.instr_vec.pop_front() {
            // Execute all the instructions, regardless of their type
            self.exec_single(entry.get_inner_instr())?;

            // If the instr was a Call, it may have copyouts to execute.
            if let InstrEntry::Call(instr_call, copyouts) = entry {
                // Only execute copyouts if the call itself was successful, otherwise the values being
                // copied out may be invalid.
                if self.was_call_successful(&instr_call)? {
                    for copyout_entry in copyouts.iter() {
                        self.exec_single(Instr::CopyOut(*copyout_entry))?;
                    }
                }
            } // No copyouts to process if the instruction was not a call
        }

        Ok(())
    }

    /// Executes the provided instruction.
    /// If the instruction is a call, the provided exec function is called with the provided input case
    /// and the results are stored in the results array if the call has a valid copyout index.
    fn exec_single(&mut self, instr: Instr) -> Result<(), DecoderError> {
        /// The number of bytes to offset all data operations by.
        const COPYIN_OFFSET: u64 = 0;

        match instr {
            Instr::CopyIn(i) => match i.arg {
                Arg::Const(a) => {
                    let size = a.meta & 0xff;
                    let bf = (a.meta >> 8) & 0xff;
                    let bf_off = (a.meta >> 16) & 0xff;
                    let bf_len = (a.meta >> 24) & 0xff;
                    let val = a.val + ((a.meta >> 32) * i.rpid);

                    copyin(
                        &mut self.mem,
                        i.wire.addr + COPYIN_OFFSET,
                        val,
                        size,
                        bf,
                        bf_off,
                        bf_len,
                    )?;
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

                    copyin(
                        &mut self.mem,
                        i.wire.addr + COPYIN_OFFSET,
                        val,
                        size,
                        bf,
                        0,
                        0,
                    )?;
                }
                Arg::Data((a, d)) => {
                    self.mem.write_mem(
                        (i.wire.addr + COPYIN_OFFSET) as usize,
                        &d.as_bytes()[..a.size as usize],
                    );
                }
            },

            Instr::CopyOut(i) => {
                let mut val = 0u64;
                copyout(&mut self.mem, i.wire.addr, i.wire.size, &mut val)?;

                let r = &self.results[i.wire.index as usize];
                // Its assumed if we're executing a CopyOut, that the associated call was
                // successful. We should not have been passed a CopyOut instruction to execute if
                // the associated call was not successful.
                r.mark_executed(true, val);
            }

            Instr::Call(i) => {
                // Evaluate all input arguments.
                let mut args = [0u64; MAX_ARGS];

                let mut skip = false;
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
                                skip = true;
                                break;
                            } else {
                                let mut v = r.val.load(Ordering::SeqCst);
                                v = v.checked_div(a.op_div).unwrap_or(v);
                                v + a.op_add
                            };

                            args[n] = val;
                        }
                        Arg::Data(_) => return Err(DecoderError::UnsupportedArgData),
                    }
                }

                let exec_result = if skip {
                    // Skipped function call because we couldn't fill out all
                    // the values needed from dependent calls
                    InputResult {
                        code: 0,
                        name: "<skipped>".into(),
                        is_success: false,
                    }
                } else {
                    // Call the provided exec function now that we've parsed an input.
                    let input_struct = InputCase {
                        call_num: i.idx as u64,
                        args,
                        num_args: i.args.len() as u64,
                        _priv: PhantomData,
                    };
                    (self.exec)(&mut self.mem, input_struct)
                };

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
                    return Err(DecoderError::OverflowOutIndex(copyout_index));
                }
                let r = &results[copyout_index];

                r.mark_executed(exec_result.is_success, exec_result.code);
            }
        }

        Ok(())
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
) -> Result<(), DecoderError> {
    if bf != 0 && (bf_off != 0 || bf_len != 0) {
        return Err(DecoderError::CopyInUnsupportedStringMask);
    }

    let tmp_str = match bf {
        // Case: binary_format_native
        0 => {
            match size {
                1 => store_by_bitmask(mem, addr, val as u8, bf_off, bf_len),
                2 => store_by_bitmask(mem, addr, val as u16, bf_off, bf_len),
                4 => store_by_bitmask(mem, addr, val as u32, bf_off, bf_len),
                8 => store_by_bitmask(mem, addr, val, bf_off, bf_len),
                _ => return Err(DecoderError::CopyInBadSize { bf, size }),
            }
            return Ok(());
        }

        // Case: binary_format_bigendian
        1 => panic!("unhandled: bigendian binary format"),

        // Case: binary_format_strdec
        // Converts 0xffffffffffffffff into 34343736`34343831
        // 35353930`37333730 00000000`35313631
        // TODO: verify endianness & correctness and re-assess implementation for perf
        2 => format!("{val:020}"),

        // Case: binary_format_strhex
        // Stores ascii of hex, e.g val 0xffffffffffffffff turns
        // into 0x66666666`66667830 66666666`66666666
        // TODO: verify endianness & correctness and re-assess implementation for perf
        3 => format!("{val:#018x}"),

        // Case: binary_format_stroct
        // turns 0xffffffffffffffff into 37373737`37373130
        // 37373737`37373737 00373737`37373737
        // TODO: verify endianness & correctness and re-assess implementation for perf
        4 => format!("{val:0023o}"),

        _ => return Err(DecoderError::CopyInBadFormat(bf)),
    };

    if size as usize != tmp_str.len() {
        return Err(DecoderError::CopyInBadSize { bf, size });
    }

    mem.try_write_mem(addr as usize, tmp_str.as_bytes())
        .map_err(DecoderError::Other)
}

fn copyout<M: SafeMemoryMap + ?Sized>(
    mem: &mut M,
    addr: u64,
    size: u64,
    res: &mut u64,
) -> Result<(), DecoderError> {
    // NB: this code makes an assumption that we are working with LSB only architectures
    const _STATIC_ASSERT_IS_LSB: [u8; (1 - u16::from_le_bytes([1, 0])) as usize] = []; // if this errors we are compiling to an MSB arch

    let mut buf = [0; 8];
    let readlen = size.min(8) as usize;
    mem.try_read_mem(addr as usize, &mut buf[0..readlen])
        .map_err(DecoderError::Other)?;
    match size {
        1 => (),
        2 => (),
        4 => (),
        8 => (),
        _ => return Err(DecoderError::CopyOutBadSize(size)),
    }

    *res = u64::from_le_bytes(buf);
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::instr::*;
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

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len).unwrap();

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

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len).unwrap();

        let expected: [u8; 4] = [0x4, 0x0, 0x0, 0x0];
        assert_eq!(mem, expected);
    }

    #[test]
    fn test_copyin_with_invalid_size() {
        let mut mem: [u8; 8] = [0; 8];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0x1234567890abcdef;
        let size: u64 = 3;
        let bf: u64 = 0; // binary_format_native
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        assert_eq!(
            copyin(&mut mem, addr, val, size, bf, bf_off, bf_len),
            Err(DecoderError::CopyInBadSize { bf: 0, size: 3 }),
        );
    }

    #[test]
    fn test_copyin_with_unknown_binary_format() {
        let mut mem: [u8; 8] = [0; 8];
        let addr = mem.as_ptr() as u64;
        let val: u64 = 0x1234567890abcdef;
        let size: u64 = 8;
        let bf: u64 = 5;
        let bf_off: u64 = 0;
        let bf_len: u64 = 0;

        assert_eq!(
            copyin(&mut mem, addr, val, size, bf, bf_off, bf_len),
            Err(DecoderError::CopyInBadFormat(5)),
        );
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

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len).unwrap();

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

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len).unwrap();

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

        copyin(&mut mem, addr, val, size, bf, bf_off, bf_len).unwrap();

        // 0xffffffffffffffff -> 37373737`37373130 37373737`37373737 00373737`37373737 (ascii oct)
        let expected: [u8; 23] = [
            0x30, 0x31, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
            0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37, 0x37,
        ];
        assert_eq!(mem, expected);
    }

    fn make_noop_prog_with_results<M: SafeMemoryMap>(
        mem: M,
        results: Arc<TestcaseResults>,
    ) -> DecodedProgram<M, fn(&mut M, InputCase) -> InputResult> {
        DecodedProgram {
            instr_vec: VecDeque::new(),
            mem,
            exec: |_, _| InputResult::default(),
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
        let mut prog = make_noop_prog_with_results(mem, results);

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
        prog.mem.read_mem(addr as usize, &mut buf);
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
        let mut prog = make_noop_prog_with_results(mem, results);

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
        prog.mem.read_mem(addr as usize, &mut buf);
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
        let mut prog = make_noop_prog_with_results(mem, results);

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
        prog.mem.read_mem(addr as usize, &mut buf);
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

        let mut prog = DecodedProgram::test_new(
            move |_, input| {
                captured_clone.store(input.args[0], Ordering::SeqCst);
                InputResult {
                    code: 0,
                    name: String::new(),
                    is_success: true,
                }
            },
            results,
        );

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

        let mut prog = DecodedProgram::test_new(
            |_, _| {
                was_called_clone.store(true, Ordering::SeqCst);
                InputResult::default()
            },
            results,
        );

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
