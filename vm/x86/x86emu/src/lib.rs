// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

mod cpu;
mod emulator;
mod registers;

pub use cpu::Cpu;
pub use emulator::fast_path;
pub use emulator::Emulator;
pub use emulator::Error;
pub use emulator::MAX_REP_LOOPS;
pub use registers::Gp;
pub use registers::GpSize;
pub use registers::RegisterIndex;
pub use registers::Segment;
