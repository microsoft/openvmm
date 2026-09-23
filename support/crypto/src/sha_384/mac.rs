// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-384 implementation using the macOS CommonCrypto API.

use std::ffi::c_int;

// Opaque storage for a CommonCrypto `CC_SHA512_CTX` (SHA-384 shares the
// SHA-512 context type in `<CommonCrypto/CommonDigest.h>`; we only need a
// buffer of the right size and alignment to back it. The header's current
// size is 216 bytes; we round up generously to leave headroom for any future
// ABI growth. `u64` storage guarantees 8-byte alignment, which exceeds
// CommonCrypto's requirement.
#[repr(C)]
struct CcSha512Ctx([u64; 48]);

// CommonCrypto is part of libSystem, which is linked by default on macOS;
// no `#[link]` attribute is required.
unsafe extern "C" {
    fn CC_SHA384_Init(ctx: *mut CcSha512Ctx) -> c_int;
    fn CC_SHA384_Update(ctx: *mut CcSha512Ctx, data: *const u8, len: u32) -> c_int;
    fn CC_SHA384_Final(md: *mut u8, ctx: *mut CcSha512Ctx) -> c_int;
}

pub struct Sha384(CcSha512Ctx);

impl Sha384 {
    pub fn new() -> Self {
        let mut ctx = CcSha512Ctx([0; 48]);
        // SAFETY: ctx is a writable, properly-sized/aligned CcSha512Ctx.
        unsafe {
            CC_SHA384_Init(&mut ctx);
        }
        Self(ctx)
    }

    pub fn update(&mut self, data: &[u8]) {
        // SAFETY: ctx is initialized and owned; data is a valid slice.
        unsafe {
            CC_SHA384_Update(&mut self.0, data.as_ptr(), data.len() as u32);
        }
    }

    pub fn finish(mut self) -> [u8; 48] {
        let mut out = [0u8; 48];
        // SAFETY: ctx is initialized and owned; out is a 48-byte buffer
        // matching the SHA-384 digest size.
        unsafe {
            CC_SHA384_Final(out.as_mut_ptr(), &mut self.0);
        }
        out
    }
}
