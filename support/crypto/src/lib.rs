// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-agnostic cryptographic primitives.
//!
//! This crate abstracts over platform-specific crypto libraries (OpenSSL on
//! Linux, BCrypt/WinCrypt on Windows, Security.framework on macOS) so that
//! callers never interact with the underlying backend directly.
//!
//! It is explicitly specialized for the needs of the OpenVMM project and is
//! not suitable for general-purpose use.

// UNSAFETY: calling BCrypt APIs on Windows, Security.framework APIs on macOS,
// and defining/referencing an extern symbol for the backend-selection
// link-time check.
#![allow(unsafe_code)]

// Backend-selection link-time check.
//
// The build script lets compilation of this crate succeed even when the
// crate's features add up to zero or multiple backends (which will
// easily happen as a side effect of feature unification across a workspace
// or when testing a crate that transitively depends on `crypto` without
// itself picking a backend). To still guarantee that a *shipping binary*
// has linked exactly one backend, we expose a check that each binary opts
// into via the [`ensure_single_backend`] macro.
//
// The check works by having the binary place a `#[used]` reference to an
// extern symbol whose definition lives in this crate, and is only emitted
// when exactly one backend is selected. If zero or multiple backends are
// selected the symbol is undefined and linking the binary fails with a
// clear, named symbol.

#[cfg(single_backend)]
#[unsafe(no_mangle)]
extern "C" fn __openvmm_crypto_ensure_single_backend__enable_exactly_one__see_support_crypto() {}

/// Emit a `#[used]` reference to a symbol that is only defined when the
/// `crypto` crate was built with exactly one backend selected. Place a call
/// to this macro in each binary that depends on `crypto` (typically gated on
/// `#[cfg(not(test))]`) so that workspace-wide `cargo test` still succeeds,
/// while a misconfigured binary fails at link time with an unresolved-symbol
/// error pointing at the missing/conflicting backend.
#[macro_export]
macro_rules! ensure_single_backend {
    () => {
        const _: () = {
            unsafe extern "C" {
                fn __openvmm_crypto_ensure_single_backend__enable_exactly_one__see_support_crypto();
            }
            #[used]
            static _CRYPTO_BACKEND_LINK_CHECK: unsafe extern "C" fn() =
                __openvmm_crypto_ensure_single_backend__enable_exactly_one__see_support_crypto;
        };
    };
}

pub mod aes_256_cbc;
pub mod aes_256_gcm;
pub mod aes_kwp;
pub mod ecdsa;
pub mod hmac_sha_256;
pub mod kbkdf;
pub mod pkcs7;
pub mod rsa;
pub mod sha_256;
pub mod sha_384;
pub mod x509;
pub mod xts_aes_256;

mod hashes;

pub use hashes::HashAlgorithm;

pub(crate) mod mac;
pub(crate) mod win;

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(openssl)]
#[derive(Clone, Debug, thiserror::Error)]
#[error("openssl error while {1}")]
pub(crate) struct BackendError(#[source] openssl::error::ErrorStack, &'static str);

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(all(native, windows))]
#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum BackendError {
    /// An error returned by a CNG or CryptoAPI function.
    #[error("windows crypto error while {1}")]
    Windows(#[source] windows_result::Error, &'static str),
    /// The input decoded successfully but is not valid or supported for the
    /// operation being attempted.
    #[error("{0}")]
    Invalid(&'static str),
}

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(symcrypt)]
#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum BackendError {
    /// An error from the SymCrypt library.
    #[error("symcrypt error while {1}")]
    SymCrypt(#[source] symcrypt::errors::SymCryptError, &'static str),
    /// The input decoded successfully but is not valid or supported for the
    /// operation being attempted.
    #[error("{0}")]
    Invalid(&'static str),
    /// An error from encoding or decoding DER.
    #[error("DER error while {1}")]
    Der(#[source] der::Error, &'static str),
}

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(all(native, target_os = "macos"))]
#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum BackendError {
    /// An OSStatus error from a Security.framework or CoreFoundation API.
    #[error("macOS crypto error while {1}")]
    OsStatus(#[source] mac::OsStatusCode, &'static str),
    /// A Security.framework or CoreFoundation API returned a null pointer
    /// without reporting an error.
    #[error("macOS crypto error while {0}: the API returned null")]
    Null(&'static str),
    /// A Security.framework API returned an error via CFErrorRef.
    #[error("macOS crypto error while {1}: {0}")]
    Sec(String, &'static str),
    /// The input decoded successfully but is not valid or supported for the
    /// operation being attempted.
    #[error("{0}")]
    Invalid(&'static str),
    /// An error from encoding or decoding DER.
    #[error("DER error while {1}")]
    Der(#[source] der::Error, &'static str),
}
