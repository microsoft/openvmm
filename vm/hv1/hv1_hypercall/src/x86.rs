// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X86-64 hypercall support.

use super::HypercallIo;
use crate::support::AsHandler;

/// An implementation of [`HypercallIo`] on X64 register state.
pub struct X64RegisterIo<T> {
    inner: T,
    is_64bit: bool,
}

impl<T: X64RegisterState> X64RegisterIo<T> {
    /// Returns a register accessor backed by `t`.
    ///
    /// Uses the 64-bit calling convention if `is_64bit`, otherwise the 32-bit
    /// one.
    pub fn new(t: T, is_64bit: bool) -> Self {
        Self { inner: t, is_64bit }
    }

    fn gp_pair(&mut self, high: X64HypercallRegister, low: X64HypercallRegister) -> u64 {
        (self.inner.gp(high) << 32) | (self.inner.gp(low) & 0xffff_ffff)
    }

    fn mask(&self, value: u64) -> u64 {
        value
            & if self.is_64bit {
                u64::MAX
            } else {
                u32::MAX as u64
            }
    }

    fn set_control(&mut self, control: u64) {
        if self.is_64bit {
            self.inner.set_gp(X64HypercallRegister::Rcx, control);
        } else {
            self.inner.set_gp(X64HypercallRegister::Rdx, control >> 32);
            self.inner
                .set_gp(X64HypercallRegister::Rax, control & u32::MAX as u64);
        }
    }
}

impl<T> AsHandler<T> for X64RegisterIo<T> {
    fn as_handler(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> AsHandler<T> for X64RegisterIo<&mut T> {
    fn as_handler(&mut self) -> &mut T {
        &mut *self.inner
    }
}

impl<T: X64RegisterState> HypercallIo for X64RegisterIo<T> {
    fn advance_ip(&mut self) {
        let rip = self.inner.rip().wrapping_add(3);
        self.inner.set_rip(self.mask(rip));
    }

    fn retry(&mut self, control: u64) {
        // Update the input control.
        self.set_control(control)

        // rip is still at the vmcall/vmmcall instruction, nothing to do.
    }

    fn control(&mut self) -> u64 {
        if self.is_64bit {
            self.inner.gp(X64HypercallRegister::Rcx)
        } else {
            self.gp_pair(X64HypercallRegister::Rdx, X64HypercallRegister::Rax)
        }
    }

    fn vtl_input(&mut self) -> u64 {
        let name = if self.is_64bit {
            X64HypercallRegister::Rax
        } else {
            X64HypercallRegister::Rcx
        };

        let value = self.inner.gp(name);
        self.mask(value)
    }

    fn set_result(&mut self, n: u64) {
        if self.is_64bit {
            self.inner.set_gp(X64HypercallRegister::Rax, n);
        } else {
            self.inner.set_gp(X64HypercallRegister::Rdx, n >> 32);
            self.inner
                .set_gp(X64HypercallRegister::Rax, n & u32::MAX as u64);
        }
    }

    fn input_gpa(&mut self) -> u64 {
        if self.is_64bit {
            self.inner.gp(X64HypercallRegister::Rdx)
        } else {
            self.gp_pair(X64HypercallRegister::Rbx, X64HypercallRegister::Rcx)
        }
    }

    fn output_gpa(&mut self) -> u64 {
        if self.is_64bit {
            self.inner.gp(X64HypercallRegister::R8)
        } else {
            self.gp_pair(X64HypercallRegister::Rdi, X64HypercallRegister::Rsi)
        }
    }

    fn fast_register_pair_count(&mut self) -> usize {
        if self.is_64bit {
            7
        } else {
            1
        }
    }

    fn extended_fast_hypercalls_ok(&mut self) -> bool {
        self.is_64bit
    }

    fn fast_input(&mut self, buf: &mut [[u64; 2]], _output_register_pairs: usize) -> usize {
        self.fast_regs(0, buf);
        buf.len()
    }

    fn fast_output(&mut self, starting_pair_index: usize, buf: &[[u64; 2]]) {
        // Continue after the input registers.
        for (i, &[low, high]) in buf.iter().enumerate() {
            let index = i + starting_pair_index;
            if index == 0 {
                self.inner.set_gp(X64HypercallRegister::Rdx, low);
                self.inner.set_gp(X64HypercallRegister::R8, high);
            } else {
                let x = low as u128 | ((high as u128) << 64);
                self.inner.set_xmm(index - 1, x)
            }
        }
    }

    fn fast_regs(&mut self, starting_pair_index: usize, buf: &mut [[u64; 2]]) {
        if self.is_64bit {
            for (i, [low, high]) in buf.iter_mut().enumerate() {
                let index = i + starting_pair_index;
                if index == 0 {
                    *low = self.inner.gp(X64HypercallRegister::Rdx);
                    *high = self.inner.gp(X64HypercallRegister::R8);
                } else {
                    let value = self.inner.xmm(index - 1);
                    *low = value as u64;
                    *high = (value >> 64) as u64;
                }
            }
        } else if let [[low, high], ..] = buf {
            *low = self.gp_pair(X64HypercallRegister::Rbx, X64HypercallRegister::Rcx);
            *high = self.gp_pair(X64HypercallRegister::Rdi, X64HypercallRegister::Rsi);
        }
    }
}

/// Register state access for x86/x64.
pub trait X64RegisterState {
    /// RIP register.
    fn rip(&mut self) -> u64;

    /// Sets the RIP register.
    fn set_rip(&mut self, rip: u64);

    /// Gets a general purpose register.
    fn gp(&mut self, n: X64HypercallRegister) -> u64;

    /// Sets a general purpose register.
    fn set_gp(&mut self, n: X64HypercallRegister, value: u64);

    /// Gets an XMM register, `n` in `0..5`.
    fn xmm(&mut self, n: usize) -> u128;

    /// Sets an XMM register, `n` in `0..5`.
    fn set_xmm(&mut self, n: usize, value: u128);
}

impl<T: X64RegisterState> X64RegisterState for &'_ mut T {
    fn rip(&mut self) -> u64 {
        (**self).rip()
    }

    fn set_rip(&mut self, rip: u64) {
        (**self).set_rip(rip)
    }

    fn gp(&mut self, n: X64HypercallRegister) -> u64 {
        (**self).gp(n)
    }

    fn set_gp(&mut self, n: X64HypercallRegister, value: u64) {
        (**self).set_gp(n, value)
    }

    fn xmm(&mut self, n: usize) -> u128 {
        (**self).xmm(n)
    }

    fn set_xmm(&mut self, n: usize, value: u128) {
        (**self).set_xmm(n, value)
    }
}

/// An x64 GP register. This just contains the subset used in the hypercall ABI.
pub enum X64HypercallRegister {
    /// RAX
    Rax,
    /// RCX
    Rcx,
    /// RDX
    Rdx,
    /// RBX
    Rbx,
    /// RSI
    Rsi,
    /// RDI
    Rdi,
    /// R8
    R8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TestHypercallIo;
    use crate::tests::TestRegisterState;

    /// Test hypercall IO for x86.
    impl<T: X64RegisterState + TestRegisterState> TestHypercallIo for X64RegisterIo<T> {
        fn get_result(&mut self) -> u64 {
            if self.is_64bit {
                self.inner.gp(X64HypercallRegister::Rax)
            } else {
                self.gp_pair(X64HypercallRegister::Rdx, X64HypercallRegister::Rax)
            }
        }

        fn set_control(&mut self, control: u64) {
            X64RegisterIo::set_control(self, control);
        }

        fn set_input_gpa(&mut self, gpa: u64) {
            if self.is_64bit {
                self.inner.set_gp(X64HypercallRegister::Rdx, gpa);
            } else {
                self.inner.set_gp(X64HypercallRegister::Rbx, gpa >> 32);
                self.inner
                    .set_gp(X64HypercallRegister::Rcx, gpa & u32::MAX as u64);
            }
        }

        fn set_output_gpa(&mut self, gpa: u64) {
            if self.is_64bit {
                self.inner.set_gp(X64HypercallRegister::R8, gpa);
            } else {
                self.inner.set_gp(X64HypercallRegister::Rdi, gpa >> 32);
                self.inner
                    .set_gp(X64HypercallRegister::Rsi, gpa & u32::MAX as u64);
            }
        }

        fn set_fast_input(&mut self, buf: &[[u64; 2]]) {
            if self.is_64bit {
                let (gp, xmm) = buf.split_at(1);
                let rdx = gp[0][0];
                let r8 = gp[0][1];
                self.inner.set_gp(X64HypercallRegister::Rdx, rdx);
                self.inner.set_gp(X64HypercallRegister::R8, r8);
                for (i, [low, high]) in xmm.iter().enumerate() {
                    let value = *low as u128 | ((*high as u128) << 64);
                    self.inner.set_xmm(i, value);
                }
            } else {
                let [low, high] = buf[0];
                self.inner.set_gp(X64HypercallRegister::Rbx, low >> 32);
                self.inner
                    .set_gp(X64HypercallRegister::Rcx, low & u32::MAX as u64);
                self.inner.set_gp(X64HypercallRegister::Rdi, high >> 32);
                self.inner
                    .set_gp(X64HypercallRegister::Rsi, high & u32::MAX as u64);
            }
        }

        fn get_fast_output(&mut self, input_register_pairs: usize, buf: &mut [[u64; 2]]) {
            // Continue after the input registers.
            for (i, [low, high]) in buf.iter_mut().enumerate() {
                if i + input_register_pairs == 0 {
                    *low = self.inner.gp(X64HypercallRegister::Rdx);
                    *high = self.inner.gp(X64HypercallRegister::R8);
                } else {
                    let x = self.inner.xmm(i + input_register_pairs - 1);
                    *low = x as u64;
                    *high = (x >> 64) as u64;
                }
            }
        }

        fn get_modified_mask(&self) -> u64 {
            self.inner.get_modified_mask()
        }

        fn clear_modified_mask(&mut self) {
            self.inner.clear_modified_mask()
        }

        fn get_io_register_mask(&self) -> u64 {
            if self.is_64bit {
                1u64 << X64HypercallRegister::Rcx as usize
                    | 1u64 << X64HypercallRegister::Rax as usize
            } else {
                1u64 << X64HypercallRegister::Rdx as usize
                    | 1u64 << X64HypercallRegister::Rax as usize
            }
        }

        fn get_name(&self) -> String {
            format!("x86_{}", if self.is_64bit { "64" } else { "32" })
        }

        fn set_vtl_input(&mut self, vtl_input: u64) {
            if self.is_64bit {
                self.inner.set_gp(X64HypercallRegister::Rax, vtl_input);
            } else {
                self.inner
                    .set_gp(X64HypercallRegister::Rcx, vtl_input >> 32);
            }
        }

        fn auto_advance_ip(&mut self) {}
    }
}
