// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! SHA-256 implementation using the macOS CommonCrypto API.

use std::ffi::c_int;

// Opaque storage for a CommonCrypto `CC_SHA256_CTX`. The real layout lives in
// `<CommonCrypto/CommonDigest.h>`; we only need a buffer of the right size and
// alignment to back it. The header's current size is 104 bytes; we round up
// generously to leave headroom for any future ABI growth. `u64` storage
// guarantees 8-byte alignment, which exceeds CommonCrypto's requirement.
#[repr(C)]
struct CcSha256Ctx([u64; 32]);

// CommonCrypto is part of libSystem, which is linked by default on macOS;
// no `#[link]` attribute is required.
unsafe extern "C" {
    fn CC_SHA256_Init(ctx: *mut CcSha256Ctx) -> c_int;
    fn CC_SHA256_Update(ctx: *mut CcSha256Ctx, data: *const u8, len: u32) -> c_int;
    fn CC_SHA256_Final(md: *mut u8, ctx: *mut CcSha256Ctx) -> c_int;
}

pub struct Sha256(CcSha256Ctx);

impl Sha256 {
    pub fn new() -> Self {
        let mut ctx = CcSha256Ctx([0; 32]);
        // SAFETY: ctx is a writable, properly-sized/aligned CcSha256Ctx.
        unsafe {
            CC_SHA256_Init(&mut ctx);
        }
        Self(ctx)
    }

    pub fn update(&mut self, data: &[u8]) {
        // SAFETY: ctx is initialized and owned; data is a valid slice.
        unsafe {
            CC_SHA256_Update(&mut self.0, data.as_ptr(), data.len() as u32);
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let mut out = [0u8; 32];
        // SAFETY: ctx is initialized and owned; out is a 32-byte buffer
        // matching the SHA-256 digest size.
        unsafe {
            CC_SHA256_Final(out.as_mut_ptr(), &mut self.0);
        }
        out
    }
}
