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

pub mod aes_256_cbc;
pub mod aes_256_gcm;
pub mod aes_key_wrap;
pub mod hmac_sha_256;
pub mod kdf;
pub mod pkcs7;
pub mod rsa;
pub mod sha_256;
pub mod x509;
pub mod xts_aes_256;

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
pub(crate) use mac::BackendError;
