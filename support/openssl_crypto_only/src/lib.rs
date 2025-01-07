// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides a macro for ensuring only `libcrypto` is linked and not `libssl`.

#![cfg(unix)]
// UNSAFETY: needed for exporting unmangled names and importing openssl
// routines.
#![expect(unsafe_code)]
#![warn(missing_docs)]
#![no_std]

use core::ffi::c_int;
use core::ffi::c_void;

unsafe extern "C" {
    #[doc(hidden)]
    pub fn OPENSSL_init_crypto(opts: u64, settings: *const c_void) -> c_int;
}

/// Ensure only libcrypto is linked by its use in the `openssl` crate, instead
/// of both libssl and libcrypto.
///
/// The `openssl` crate calls `OPENSSL_init_ssl` unconditionally, which
/// initializes both libcrypto and libssl.
///
/// This module redefines `OPENSSL_init_ssl` so that it calls
/// `OPENSSL_init_crypto` instead. As a result, the linker will skip pulling in
/// and initializing libssl.
#[macro_export]
macro_rules! openssl_crypto_only {
    () => {
        /// # Safety
        ///
        /// The caller must call as documented for `OPENSSL_init_ssl`.
        // SAFETY: We are purposefully overriding this symbol and we have made
        // sure the definition is compatible with the original.
        #[unsafe(no_mangle)]
        unsafe extern "C" fn OPENSSL_init_ssl(
            opts: u64,
            settings: *const ::core::ffi::c_void,
        ) -> ::core::ffi::c_int {
            // SAFETY: this method has the same interface as `OPENSSL_init_ssl`, so this
            // is guaranteed by the caller.
            unsafe { $crate::OPENSSL_init_crypto(opts, settings) }
        }
    };
}
