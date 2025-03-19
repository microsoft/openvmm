// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: needed to write low-level TMK code.
#![expect(unsafe_code)]

mod aarch64;
mod x86;

use core::marker::PhantomData;

#[repr(C)]
struct Str<'a>(*const u8, usize, PhantomData<&'a str>);

// SAFETY: `Str` is an ABI-safe type for &str, which is Send+Sync.
unsafe impl Send for Str<'_> {}
// SAFETY: `Str` is an ABI-safe type for &str, which is Send+Sync.
unsafe impl Sync for Str<'_> {}

impl<'a> Str<'a> {
    const fn new(s: &'a str) -> Self {
        Self(s.as_ptr(), s.len(), PhantomData)
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    minimal_rt::arch::fault();
}
