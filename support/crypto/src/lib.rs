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

// UNSAFETY: calling BCrypt APIs on Windows, Security.framework APIs on macOS.
#![allow(unsafe_code)]

// When multiple crypto backends are enabled at once (typically via Cargo
// feature unification across the workspace), `cargo check` is allowed to
// succeed so that workspace-wide checks aren't blocked, but linking must
// fail with a clear message. We achieve this by referencing an undefined
// extern symbol from a `#[used]` static, which `cargo check` ignores
// (it doesn't link) but `cargo build` resolves and fails on with an
// unresolved-symbol error naming the symbol below.
#[cfg(multi_backend)]
const _: () = {
    unsafe extern "C" {
        fn __openvmm_crypto_multiple_backends_enabled__enable_exactly_one__see_support_crypto();
    }
    #[used]
    static _FORCE_LINK_ERROR: unsafe extern "C" fn() =
        __openvmm_crypto_multiple_backends_enabled__enable_exactly_one__see_support_crypto;
};

pub mod aes_256_cbc;
pub mod aes_256_gcm;
pub mod aes_kwp;
pub mod hmac_sha_256;
pub mod kbkdf;
pub mod pkcs7;
pub mod rsa;
pub mod sha_256;
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
#[error("openssl error during {1}")]
pub(crate) struct BackendError(#[source] openssl::error::ErrorStack, &'static str);

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(all(native, windows))]
#[derive(Clone, Debug, thiserror::Error)]
#[error("windows crypto error during {1}")]
pub(crate) struct BackendError(#[source] windows_result::Error, &'static str);

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(symcrypt)]
#[derive(Clone, Debug, thiserror::Error)]
#[error("symcrypt backend error during {1}")]
pub(crate) enum BackendError {
    /// An error from the SymCrypt library, with the operation being performed when the error occurred.
    SymCrypt(#[source] symcrypt::errors::SymCryptError, &'static str),
    /// An error from encoding or decoding PKCS#8, with the operation being performed when the error occurred.
    Pkcs8Encoding(#[source] pkcs8::Error, &'static str),
    /// An error from DER encoding or decoding, with the operation being performed when the error occurred.
    Der(#[source] der::Error, &'static str),
}

#[cfg(all(native, target_os = "macos"))]
#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum BackendError {
    /// An OSStatus error from a Security.framework or CoreFoundation API.
    #[error("{1} returned a failure status code")]
    OsStatus(#[source] mac::OsStatusCode, &'static str),
    /// A Security.framework or CoreFoundation API returned a null pointer.
    #[error("{0}: returned null")]
    Null(&'static str),
    /// A Security.framework API returned an error via CFErrorRef.
    #[error("Security.framework error during {1}: {0}")]
    Sec(String, &'static str),
    /// An error from encoding or decoding PKCS#8.
    #[error("PKCS#8 error during {1}")]
    Pkcs8(#[source] pkcs8::Error, &'static str),
    /// An error from DER encoding or decoding.
    #[error("DER error during {1}")]
    Der(#[source] der::Error, &'static str),
}
