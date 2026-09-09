use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;

use alloc::{string::String, vec::Vec};
use zerocopy::FromBytes;
use zerocopy::SizeError;

use crate::instr::*;
use crate::wire;

/// An enumeration of possible decoder errors
#[derive(Debug, PartialEq)]
pub enum DecoderError {
    /// A bad magic value for the program header
    BadMagic(u64),
    /// The program size is zero
    ZeroProgSize,
    /// The program size exceeds the remaining size of the buffer
    ProgSizeExceedsBuffer,
    /// A truncated read occurred
    TruncRead {
        /// The context of the value being read
        context: &'static str,
        /// The number of bytes expected to read
        expected: usize,
        /// The actual number of bytes read
        got: usize,
    },
    /// A bad instruction code
    BadInstruction(i64),
    /// A bad argument code
    BadArgType(u64),
    /// The data of an argument is zero size
    ZeroDataSize,
    /// copy_in: a bitmask is non-zero for string binary format
    CopyInUnsupportedStringMask,
    /// copy_in: bad size value for a particular binary format
    CopyInBadSize {
        /// The binary format code
        bf: u64,
        /// The size that was set for this
        size: u64,
    },
    /// copy_in: bad format type
    CopyInBadFormat(u64),
    /// copy_out: bad size value
    CopyOutBadSize(u64),
    /// A data argument is passed to a function call argument directly
    /// (unsupported)
    UnsupportedArgData,
    /// The copyout_index overflows/underflows K_MAX_COMMANDS
    OverflowOutIndex(usize),
    /// Some other error not captured here
    Other(String),
}

impl DecoderError {
    fn trunc<D>(context: &'static str, value: SizeError<&[u8], D>) -> Self {
        Self::TruncRead {
            context,
            expected: size_of::<D>(),
            got: value.into_src().len(),
        }
    }
}

pub(crate) struct Decoder<'a> {
    hdr: wire::ProgramHeader,
    buf: &'a [u8],
    custom_results_counter: AtomicUsize,
}

impl<'a> Decoder<'a> {
    pub(crate) fn new(mut buf: &'a [u8]) -> Result<Self, DecoderError> {
        let hdr = read_struct::<wire::ProgramHeader>("program header", &mut buf)?;

        if hdr.magic != 0xbadc0ffeebadface {
            return Err(DecoderError::BadMagic(hdr.magic));
        } else if hdr.prog_size == 0 {
            return Err(DecoderError::ZeroProgSize);
        } else if hdr.prog_size > buf.len() as u64 {
            return Err(DecoderError::ProgSizeExceedsBuffer);
        }

        Ok(Self {
            // Resize the buffer to the program size.
            buf: &buf[..hdr.prog_size as usize],
            hdr,
            custom_results_counter: AtomicUsize::new(0),
        })
    }

    /// Fetch and decode the next instruction. Returns `None` if we have reached EOF.
    pub fn try_next(&mut self) -> Result<Option<Instr>, DecoderError> {
        let n = read_inc_input("instruction type", &mut self.buf)? as i64;
        match n {
            wire::INSTR_EOF => Ok(None),

            wire::INSTR_COPYIN => Ok(Some(Instr::CopyIn(InstrCopyIn {
                rpid: self.hdr.pid,
                wire: read_struct("instruction", &mut self.buf)?,
                arg: read_arg(&mut self.buf)?,
            }))),

            wire::INSTR_COPYOUT => Ok(Some(Instr::CopyOut(InstrCopyOut {
                rpid: self.hdr.pid,
                wire: read_struct("instruction", &mut self.buf)?,
            }))),

            c if c < 0 => Err(DecoderError::BadInstruction(n)),

            _ => {
                let insn = read_struct::<wire::InstrCall>("instruction", &mut self.buf)?;

                let mut args = Vec::new();
                for _i in 0..insn.num_args {
                    args.push(read_arg(&mut self.buf)?);
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

/// Read a 64-bit little-endian word from a slice and advance the slice's pointer.
fn read_inc_input(ctx: &'static str, input_data: &mut &[u8]) -> Result<u64, DecoderError> {
    let (s, t) = u64::read_from_prefix(input_data).map_err(|e| DecoderError::trunc(ctx, e))?;
    *input_data = t;
    Ok(s)
}

/// Read a struct out of `input_data` and advance the slice's pointer.
fn read_struct<T: FromBytes>(ctx: &'static str, input_data: &mut &[u8]) -> Result<T, DecoderError> {
    let (s, t) = T::read_from_prefix(input_data).map_err(|e| DecoderError::trunc(ctx, e))?;
    *input_data = t;
    Ok(s)
}

/// Read out an argument from an input buffer.
fn read_arg(buf: &mut &[u8]) -> Result<Arg, DecoderError> {
    let typ = read_inc_input("argument type", buf)?;

    Ok(match typ {
        wire::ARG_CONST => Arg::Const(read_struct("argument", buf)?),
        wire::ARG_RESULT => Arg::Result(read_struct("argument", buf)?),
        wire::ARG_DATA => {
            let arg = read_struct::<wire::ArgData>("argument", buf)?;

            // Read out each data word.
            let cnt = arg.size.div_ceil(8);
            if cnt == 0 {
                return Err(DecoderError::ZeroDataSize);
            }

            let mut words = Vec::new();
            for _i in 0..cnt {
                words.push(read_inc_input("argument data", buf)?);
            }

            Arg::Data((arg, words))
        }
        // arg_csum
        0x3 => todo!(),
        // Catchall
        _ => return Err(DecoderError::BadArgType(typ)),
    })
}
