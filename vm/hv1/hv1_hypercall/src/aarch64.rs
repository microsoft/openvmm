// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ARM64 hypercall support.

use super::HypercallIo;
use crate::support::AsHandler;

/// Provides access to the ARM64 register state needed to parse hypercalls.
pub trait Arm64RegisterState {
    /// Gets the program counter.
    fn pc(&mut self) -> u64;
    /// Sets the program counter.
    fn set_pc(&mut self, pc: u64);
    /// Gets register Xn, `n <= 17`.
    fn x(&mut self, n: u8) -> u64;
    /// Sets register Xn, `n <= 17`.
    fn set_x(&mut self, n: u8, v: u64);
}

impl<T: Arm64RegisterState> Arm64RegisterState for &'_ mut T {
    fn pc(&mut self) -> u64 {
        (**self).pc()
    }

    fn set_pc(&mut self, pc: u64) {
        (**self).set_pc(pc)
    }

    fn x(&mut self, n: u8) -> u64 {
        (**self).x(n)
    }

    fn set_x(&mut self, n: u8, v: u64) {
        (**self).set_x(n, v)
    }
}

/// An implementation of [`HypercallIo`] on top of [`Arm64RegisterState`].
pub struct Arm64RegisterIo<T> {
    inner: T,
    pre_advanced: bool,
    smccc: bool,
}

impl<T> AsHandler<T> for Arm64RegisterIo<T> {
    fn as_handler(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> AsHandler<T> for Arm64RegisterIo<&mut T> {
    fn as_handler(&mut self) -> &mut T {
        &mut *self.inner
    }
}

impl<T: Arm64RegisterState> Arm64RegisterIo<T> {
    /// Returns a new instance.
    ///
    /// If `pre_advanced`, the PC has already been advanced (which the ARM
    /// processor does automatically when the HVC instruction is executed).
    ///
    /// If `smccc`, this is the SMCCC calling convention (hvc #0). Otherwise, it
    /// is the Hyper-V calling convention (hvc #1).
    pub fn new(t: T, pre_advanced: bool, smccc: bool) -> Self {
        Self {
            inner: t,
            pre_advanced,
            smccc,
        }
    }

    fn set_control(&mut self, control: u64) {
        // X0 for Hyper-V, X1 for SMCCC.
        self.inner.set_x(self.smccc as u8, control);
    }
}

impl<T: Arm64RegisterState> HypercallIo for Arm64RegisterIo<T> {
    fn advance_ip(&mut self) {
        if !self.pre_advanced {
            let pc = self.inner.pc().wrapping_add(4);
            self.inner.set_pc(pc);
        }
    }

    fn retry(&mut self, control: u64) {
        self.set_control(control);
        if self.pre_advanced {
            let pc = self.inner.pc().wrapping_sub(4);
            self.inner.set_pc(pc);
            self.pre_advanced = false;
        }
    }

    fn control(&mut self) -> u64 {
        // X0 for Hyper-V, X1 for SMCCC.
        self.inner.x(self.smccc as u8)
    }

    fn input_gpa(&mut self) -> u64 {
        // X1 for Hyper-V, X2 for SMCCC.
        self.inner.x(1 + self.smccc as u8)
    }

    fn output_gpa(&mut self) -> u64 {
        // X2 for Hyper-V, X3 for SMCCC.
        self.inner.x(2 + self.smccc as u8)
    }

    fn fast_register_pair_count(&mut self) -> usize {
        8
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        true
    }

    fn fast_input(&mut self, buf: &mut [[u64; 2]], output_register_pairs: usize) -> usize {
        self.fast_regs(0, buf);

        if self.smccc {
            // SMCCC: start output after the input registers.
            buf.len()
        } else {
            // Hyper-V: use the last n registers for output.
            self.fast_register_pair_count() - output_register_pairs
        }
    }

    fn fast_output(&mut self, starting_pair_index: usize, buf: &[[u64; 2]]) {
        // X1-X16 for Hyper-V, X2-X17 for SMCCC.
        let start = starting_pair_index * 2 + 1 + self.smccc as usize;

        for (i, &[low, high]) in buf.iter().enumerate() {
            self.inner.set_x((start + i * 2) as u8, low);
            self.inner.set_x((start + i * 2 + 1) as u8, high);
        }
    }

    fn vtl_input(&mut self) -> u64 {
        0
    }

    fn set_result(&mut self, n: u64) {
        // Always X0.
        self.inner.set_x(0, n)
    }

    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        // X1-X16 for Hyper-V, X2-X17 for SMCCC.
        let start = starting_pair_index * 2 + 1 + self.smccc as usize;
        for (i, [low, high]) in buf.iter_mut().enumerate() {
            *low = self.inner.x((start + i * 2) as u8);
            *high = self.inner.x((start + i * 2 + 1) as u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestHypercallIo;
    use crate::tests::TestRegisterState;

    /// Test hypercall IO for ARM64.
    impl<T: Arm64RegisterState + TestRegisterState> TestHypercallIo for Arm64RegisterIo<T> {
        fn get_result(&mut self) -> u64 {
            // Always X0.
            self.inner.x(0)
        }

        fn set_control(&mut self, control: u64) {
            Arm64RegisterIo::set_control(self, control);
        }

        fn set_input_gpa(&mut self, gpa: u64) {
            // X1 for Hyper-V, X2 for SMCCC.
            self.inner.set_x(1 + self.smccc as u8, gpa)
        }

        fn set_output_gpa(&mut self, gpa: u64) {
            // X2 for Hyper-V, X3 for SMCCC.
            self.inner.set_x(2 + self.smccc as u8, gpa);
        }

        fn set_fast_input(&mut self, buf: &[[u64; 2]]) {
            // X1-X16 for Hyper-V, X2-X17 for SMCCC.
            for (i, [low, high]) in buf.iter().enumerate() {
                self.inner.set_x(i as u8 * 2 + 1 + self.smccc as u8, *low);
                self.inner.set_x(i as u8 * 2 + 2 + self.smccc as u8, *high);
            }
        }

        fn get_fast_output(&mut self, input_register_pairs: usize, buf: &mut [[u64; 2]]) {
            let start = if self.smccc {
                // SMCCC: start after the input registers.
                2 + input_register_pairs * 2
            } else {
                // Hyper-V: use the last n registers, ending with X16.
                17 - buf.len() * 2
            };
            for (i, [low, high]) in buf.iter_mut().enumerate() {
                *low = self.inner.x((start + i * 2) as u8);
                *high = self.inner.x((start + i * 2 + 1) as u8);
            }
        }

        fn get_modified_mask(&self) -> u64 {
            self.inner.get_modified_mask()
        }

        fn clear_modified_mask(&mut self) {
            self.inner.clear_modified_mask()
        }

        fn get_io_register_mask(&self) -> u64 {
            // X0 is always output, control is either X0 (for Hyper-V) or X1 (for SMCCC).
            1u64 << (self.smccc as u8) | 1
        }

        fn get_name(&self) -> String {
            format!(
                "Arm64RegisterIo<pre_advanced={}, smccc={}>",
                self.pre_advanced, self.smccc
            )
        }

        fn set_vtl_input(&mut self, vtl_input: u64) {
            // No VTL input for ARM64.
            assert_eq!(vtl_input, 0);
        }

        fn auto_advance_ip(&mut self) {
            if self.pre_advanced {
                let pc = self.inner.pc().wrapping_add(4);
                self.inner.set_pc(pc);
            }
        }
    }
}
