// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_arch = "x86_64")]

use super::Context;
use super::Fault;
use super::recovery_descriptor;

pub(super) fn get_context_ip(ctx: &Context) -> usize {
    #[cfg(target_os = "linux")]
    {
        ctx.gregs[libc::REG_RIP as usize] as _
    }
    #[cfg(target_os = "windows")]
    {
        ctx.Rip as _
    }
}

pub(super) fn set_context_ip_and_result(ctx: &mut Context, ip: usize, result: Option<isize>) {
    // This function also clears the direction flag to restore the ABI expectation.
    const DIRECTION_FLAG_MASK: u32 = 0x400;
    #[cfg(target_os = "linux")]
    {
        ctx.gregs[libc::REG_RIP as usize] = ip as _;
        if let Some(result) = result {
            ctx.gregs[libc::REG_RCX as usize] = result as _;
        }
        ctx.gregs[libc::REG_EFL as usize] &= !(DIRECTION_FLAG_MASK as libc::greg_t);
    }
    #[cfg(target_os = "windows")]
    {
        ctx.Rip = ip as _;
        if let Some(result) = result {
            ctx.Rcx = result as _;
        }
        ctx.EFlags &= !DIRECTION_FLAG_MASK;
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
                2:
                mov {s1}, byte ptr [{src} + {i}]
                mov byte ptr [{dest} + {i}], {s1}
                inc {i}
                cmp {i}, {len}
                jne 2b
                3:
                ",
                recovery_descriptor!("2b", "3b", "{bail}"),
                s1 = out(reg_byte) _,
                i = inout(reg) 0u64 => _,
                src = in(reg) src,
                dest = in(reg) dest,
                len = in(reg) length,
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
                2:
                mov {s1}, qword ptr [{src} + {i}]
                mov qword ptr [{dest} + {i}], {s1}
                add {i}, 8
                cmp {i}, {len}
                jne 2b
                3:
                ",
                recovery_descriptor!("2b", "3b", "{bail}"),
                s1 = out(reg) _,
                i = inout(reg) 0u64 => _,
                src = in(reg) src,
                dest = in(reg) dest,
                len = in(reg) length,
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
                2:
                movdqu {s1}, xmmword ptr [{src} + {i}]
                movdqu {s2}, xmmword ptr [{src} + {i} + 16]
                movdqu xmmword ptr [{dest} + {i}], {s1}
                movdqu xmmword ptr [{dest} + {i} + 16], {s2}
                add {i}, 32
                cmp {i}, {len}
                jne 2b
                3:
                ",
                recovery_descriptor!("2b", "3b", "{bail}"),
                s1 = out(xmm_reg) _,
                s2 = out(xmm_reg) _,
                i = inout(reg) 0u64 => _,
                src = in(reg) src,
                dest = in(reg) dest,
                len = in(reg) length,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    fn copy_movsb(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "2:",
                "rep movsb",
                "3:",
                recovery_descriptor!("2b", "3b", "{bail}"),
                inout("rdi") dest => _,
                inout("rsi") src => _,
                inout("rcx") length => _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    if length >= 1024 {
        return copy_movsb(dest, src, length);
    }
    crate::memcpy::try_copy_forward_with(dest, src, length, copy1, copy8, copy32)
}

/// # Safety
/// `dest` must be an address that's reserved and can be written to without
/// violating Rust's aliasing rules. `src` must be an address that's reserved.
unsafe fn try_copy_backward(dest: *mut u8, src: *const u8, length: usize) -> Result<(), Fault> {
    // Note, `rep movsb` with the direction flag set is slow, but this path
    // should be rare.
    // SAFETY: caller ensured.
    unsafe {
        core::arch::asm! {
            "2:",
            "std",
            "rep movsb",
            "cld", // note: `set_context_ip_and_result` will clear this in the failure case
            "3:",
            recovery_descriptor!("2b", "3b", "{bail}"),
            inout("rdi") dest.add(length - 1) => _,
            inout("rsi") src.add(length - 1) => _,
            inout("rcx") length => _,
            bail = label { return Err(Fault) },
            options(nostack),
        }
    }
    Ok(())
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
    fn set_stosb(dest: *mut u8, c: u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "2:",
                "rep stosb",
                "3:",
                recovery_descriptor!("2b", "3b", "{bail}"),
                inout("rdi") dest => _,
                in("al") c,
                inout("rcx") length => _,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    fn set1(dest: *mut u8, c: u8, length: usize) -> Result<(), Fault> {
        // SAFETY: caller ensured.
        unsafe {
            core::arch::asm! {
                "
                2:
                mov byte ptr [{dest} + {i}], {c}
                inc {i}
                cmp {i}, {len}
                jne 2b
                3:
                ",
                recovery_descriptor!("2b", "3b", "{bail}"),
                c = in(reg_byte) c,
                i = inout(reg) 0u64 => _,
                dest = in(reg) dest,
                len = in(reg) length,
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
                2:
                mov qword ptr [{dest} + {i}], {c}
                add {i}, 8
                cmp {i}, {len}
                jne 2b
                3:
                ",
                recovery_descriptor!("2b", "3b", "{bail}"),
                c = in(reg) c as u64 * 0x0101010101010101,
                i = inout(reg) 0u64 => _,
                dest = in(reg) dest,
                len = in(reg) length,
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
                2:
                movdqu xmmword ptr [{dest} + {i}], {c}
                movdqu xmmword ptr [{dest} + {i} + 16], {c}
                add {i}, 32
                cmp {i}, {len}
                jne 2b
                3:
                ",
                recovery_descriptor!("2b", "3b", "{bail}"),
                c = in(xmm_reg) 0,
                i = inout(reg) 0u64 => _,
                dest = in(reg) dest,
                len = in(reg) length,
                bail = label { return Err(Fault) },
                options(nostack),
            }
        }
        Ok(())
    }

    if length >= 1024 {
        return set_stosb(dest, c, length);
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
                    "2:",
                    $asm,
                    "xor ecx, ecx",
                    "3:",
                    recovery_descriptor!("2b", "3b", "."),
                    out = out(reg) out,
                    src = in(reg) src,
                    lateout("rcx") result,
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

try_read!(pub(crate) try_read8, u8, "movzx {out:e}, byte ptr [{src}]");
try_read!(pub(crate) try_read16, u16, "movzx {out:e}, word ptr [{src}]");
try_read!(pub(crate) try_read32, u32, "mov {out:e}, dword ptr [{src}]");
try_read!(pub(crate) try_read64, u64, "mov {out:r}, qword ptr [{src}]");

macro_rules! try_write {
    ($vis:vis $func:ident, $ty:ty, $asm:expr) => {
        /// # Safety
        /// `dest` must be an address that's reserved and can be written to
        /// without violating Rust's aliasing rules.
        $vis unsafe fn $func(dest: *mut $ty, val: $ty) -> Result<(), Fault> {
            // SAFETY: caller ensured.
            unsafe {
                core::arch::asm!(
                    "2:",
                    $asm,
                    "3:",
                    recovery_descriptor!("2b", "3b", "{bail}"),
                    dest = in(reg) dest,
                    val = in(reg) val as u64,
                    bail = label { return Err(Fault) },
                    options(nostack, preserves_flags),
                )
            }
            Ok(())
        }
    };
}

try_write!(pub(crate) try_write8, u8, "mov byte ptr [{dest}], {val:l}");
try_write!(pub(crate) try_write16, u16, "mov word ptr [{dest}], {val:x}");
try_write!(pub(crate) try_write32, u32, "mov dword ptr [{dest}], {val:e}");
try_write!(pub(crate) try_write64, u64, "mov qword ptr [{dest}], {val:r}");

macro_rules! try_cmpxchg {
    ($vis:vis $func:ident, $ty:ty, $ax:tt, $reg_kind:tt, $asm:expr) => {
        /// # Safety
        /// `dest` must be an address that's reserved and can be written to
        /// without violating Rust's aliasing rules.
        $vis unsafe fn $func(
            dest: *mut $ty,
            expected: &mut $ty,
            desired: $ty,
        ) -> Result<bool, Fault> {
            let actual;
            let result: i8;
            // SAFETY: caller ensured.
            unsafe {
                core::arch::asm! {
                    "2:",
                    $asm,
                    "setz cl",
                    "3:",
                    recovery_descriptor!("2b", "3b", "."),
                    dest = in(reg) dest,
                    desired = in($reg_kind) desired,
                    inout($ax) *expected => actual,
                    lateout("cl") result,
                    options(nostack),
                }
            };
            if result > 0 {
                Ok(true)
            } else if result == 0 {
                *expected = actual;
                Ok(false)
            } else {
                Err(Fault)
            }
        }
    };
}

try_cmpxchg!(pub(crate) try_cmpxchg8, u8, "al", reg_byte, "lock cmpxchg byte ptr [{dest}], {desired}");
try_cmpxchg!(pub(crate) try_cmpxchg16, u16, "ax", reg, "lock cmpxchg word ptr [{dest}], {desired:x}");
try_cmpxchg!(pub(crate) try_cmpxchg32, u32, "eax", reg, "lock cmpxchg dword ptr [{dest}], {desired:e}");
try_cmpxchg!(pub(crate) try_cmpxchg64, u64, "rax", reg, "lock cmpxchg qword ptr [{dest}], {desired:r}");
