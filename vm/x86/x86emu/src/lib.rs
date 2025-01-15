// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

mod cpu;
mod emulator;
mod registers;

pub use cpu::Cpu;
pub use emulator::fast_path;
pub use emulator::Emulator;
pub use registers::bitness;
pub use registers::Bitness;
pub use registers::{Cr0, Efer, Gp, Rip, Xmm};
pub use emulator::Error;
pub use emulator::MAX_REP_LOOPS;
pub use registers::CpuState;
