// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "aarch64")]

use super::Context;
use super::Fault;
use super::recovery_descriptor;

pub(super) fn get_context_ip(ctx: &Context) -> usize {
    #[cfg(target_os = "linux")]
    {
        ctx.pc as _
    }
    #[cfg(target_os = "macos")]
    {
        ctx.__ss.__pc as _
    }
    #[cfg(windows)]
    {
        ctx.Pc as _
    }
}

pub(super) fn set_context_ip_and_result(ctx: &mut Context, ip: usize, result: Option<isize>) {
    #[cfg(target_os = "linux")]
    {
        ctx.pc = ip as _;
        if let Some(result) = result {
            ctx.regs[0] = result as _;
        }
    }
    #[cfg(target_os = "macos")]
    {
        ctx.__ss.__pc = ip as _;
        if let Some(result) = result {
            ctx.__ss.__x[0] = result as _;
        }
    }
    #[cfg(windows)]
    {
        ctx.Pc = ip as _;
        if let Some(result) = result {
            // SAFETY: the union is always valid.
            unsafe { ctx.Anonymous.X[0] = result as _ };
        }
    }
}

/// # Safety
/// `dest` must be an address that's reserved and can be written to without
/// violating Rust's aliasing rules. `src` must be an address that's reserved.
unsafe fn try_copy_forward(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
    fn copy1(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                1:
                ldrb {s1:w}, [{src}], #1
                subs {len}, {len}, #1
                strb {s1:w}, [{dest}], #1
                bne 1b
                2:",
                recovery_descriptor!("1b", "2b", "{bail}"),
                dest = inout(reg) dest => _,
                src = inout(reg) src => _,
                len = inout(reg) length => _,
                s1 = out(reg) _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    fn copy8(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                1:
                ldr {s1:x}, [{src}], #8
                subs {len}, {len}, #8
                str {s1:x}, [{dest}], #8
                bne 1b
                2:",
                recovery_descriptor!("1b", "2b", "{bail}"),
                dest = inout(reg) dest => _,
                src = inout(reg) src => _,
                len = inout(reg) length => _,
                s1 = out(reg) _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    fn copy32(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                1:
                ldr {s1:q}, [{src}], #16
                ldr {s2:q}, [{src}], #16
                subs {len}, {len}, #32
                str {s1:q}, [{dest}], #16
                str {s2:q}, [{dest}], #16
                bne 1b
                2:",
                recovery_descriptor!("1b", "2b", "{bail}"),
                dest = inout(reg) dest => _,
                src = inout(reg) src => _,
                len = inout(reg) length => _,
                s1 = out(vreg) _,
                s2 = out(vreg) _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    crate::memcpy::try_copy_forward_with(dest, src, length, copy1, copy8, copy32)
}

/// # Safety
/// `dest` must be an address that's reserved and can be written to without
/// violating Rust's aliasing rules. `src` must be an address that's reserved.
unsafe fn try_copy_backward(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
    // SAFETY: caller ensured.
    unsafe {
        core::arch::asm! {
            "
            cbz {len}, 2f
            sub {dest}, {dest}, #1
            sub {src}, {src}, #1
            1:
            ldrb {s1:w}, [{src}, {len}]
            strb {s1:w}, [{dest}, {len}]
            subs {len}, {len}, #1
            bne 1b
            2:
            ",
            recovery_descriptor!("1b", "2b", "{bail}"),
            dest = inout(reg) dest => _,
            src = inout(reg) src => _,
            len = inout(reg) length => _,
            s1 = out(reg) _,
            bail = label { return Err(Fault) },
            options(nostack),
        }
        Ok(())
    }
}

/// # Safety
/// `dest` must be an address that's reserved and can be written to without
/// violating Rust's aliasing rules. `src` must be an address that's reserved.
pub(crate) unsafe fn try_memmove(
    dest: *mut u8,
    src: *const u8,
    length: usize,
) -> Result<(), Fault> {
    if (dest as usize).wrapping_sub(src as usize) >= length {
        // SAFETY: caller ensured.
        unsafe { try_copy_forward(dest, src, length) }
    } else {
        crate::cold_path();
        // SAFETY: caller ensured.
        unsafe { try_copy_backward(dest, src, length) }
    }
}

/// # Safety
/// `dest` must be an address that's reserved and can be written to without
/// violating Rust's aliasing rules.
pub(crate) unsafe fn try_memset(dest: *mut u8, c: u8, length: usize) -> Result<(), Fault> {
    fn set1(dest: *mut u8, c: u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                1:
                strb {c:w}, [{dest}], #1
                subs {len}, {len}, #1
                bne 1b
                2:",
                recovery_descriptor!("1b", "2b", "{bail}"),
                dest = inout(reg) dest => _,
                c = in(reg) c,
                len = inout(reg) length => _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    fn set8(dest: *mut u8, c: u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                1:
                str {c:x}, [{dest}], #8
                subs {len}, {len}, #8
                bne 1b
                2:",
                recovery_descriptor!("1b", "2b", "{bail}"),
                dest = inout(reg) dest => _,
                c = in(reg) c as u64 * 0x0101010101010101,
                len = inout(reg) length => _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    fn set32_zero(dest: *mut u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                1:
                str {zero:q}, [{dest}], #16
                str {zero:q}, [{dest}], #16
                subs {len}, {len}, #32
                bne 1b
                2:",
                recovery_descriptor!("1b", "2b", "{bail}"),
                dest = inout(reg) dest => _,
                zero = in(vreg) 0,
                len = inout(reg) length => _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    crate::memcpy::try_memset_with(dest, c, length, set1, set8, set32_zero)
}

macro_rules! try_read {
    ($vis:vis $func:ident, $ty:ty, $asm:expr) => {
        /// # Safety
        /// `src` must be an address that's reserved.
        $vis unsafe fn $func(src: *const $ty) -> Result<$ty, Fault> {
            // SAFETY: caller ensured.
            unsafe {
                let out: u64;
                let result: i32;
                core::arch::asm!(
                    "1:",
                    $asm,
                    "mov w0, wzr",
                    "2:",
                    recovery_descriptor!("1b", "2b", "."),
                    out = out(reg) out,
                    src = in(reg) src,
                    lateout("x0") result,
                    options(nostack, readonly),
                );
                if result == 0 {
                    Ok(out as $ty)
                } else {
                    Err(Fault)
                }
            }
        }
    };
}

try_read!(pub(crate) try_read8, u8, "ldrb {out:w}, [{src}]");
try_read!(pub(crate) try_read16, u16, "ldrh {out:w}, [{src}]");
try_read!(pub(crate) try_read32, u32, "ldr {out:w}, [{src}]");
try_read!(pub(crate) try_read64, u64, "ldr {out:x}, [{src}]");

macro_rules! try_write {
    ($vis:vis $func:ident, $ty:ty, $asm:expr) => {
        /// # Safety
        /// `dest` must be an address that's reserved and can be written to without
        /// violating Rust's aliasing rules.
        $vis unsafe fn $func(dest: *mut $ty, val: $ty) -> Result<(), Fault> {
            // SAFETY: caller ensured.
            unsafe {
                core::arch::asm!(
                    "1:",
                    $asm,
                    "2:",
                    recovery_descriptor!("1b", "2b", "{bail}"),
                    dest = in(reg) dest,
                    val = in(reg) val as u64,
                        bail = label { return Err(Fault) },
                    options(nostack),
                )
            }
            Ok(())
        }
    };
}

try_write!(pub(crate) try_write8, u8, "strb {val:w}, [{dest}]");
try_write!(pub(crate) try_write16, u16, "strh {val:w}, [{dest}]");
try_write!(pub(crate) try_write32, u32, "str {val:w}, [{dest}]");
try_write!(pub(crate) try_write64, u64, "str {val:x}, [{dest}]");

macro_rules! try_cmpxchg {
    ($vis:vis $func:ident, $ty:ty, $asm:expr) => {
        /// # Safety
        /// `dest` must be an address that's reserved and can be written to without
        /// violating Rust's aliasing rules.
        $vis unsafe fn $func(
            dest: *mut $ty,
            expected: &mut $ty,
            desired: $ty,
        ) -> Result<bool, Fault> {
            let actual;
            let result: i32;
            // SAFETY: caller ensured.
            unsafe {
                core::arch::asm! {
                    "1:",
                    $asm,
                    "mov w0, wzr",
                    "2:",
                    recovery_descriptor!("1b", "2b", "."),
                    dest = in(reg) dest,
                    desired = in(reg) desired,
                    expected = inout(reg) *expected => actual,
                    lateout("x0") result,
                    options(nostack),
                }
            };
            if result == 0 {
                if *expected == actual {
                    Ok(true)
                } else {
                    *expected = actual;
                    Ok(false)
                }
            } else {
                Err(Fault)
            }
        }
    }
}

try_cmpxchg!(pub(crate) try_cmpxchg8, u8, "casalb {expected:w}, {desired:w}, [{dest}]");
try_cmpxchg!(pub(crate) try_cmpxchg16, u16, "casalh {expected:w}, {desired:w}, [{dest}]");
try_cmpxchg!(pub(crate) try_cmpxchg32, u32, "casal {expected:w}, {desired:w}, [{dest}]");
try_cmpxchg!(pub(crate) try_cmpxchg64, u64, "casal {expected:x}, {desired:x}, [{dest}]");
