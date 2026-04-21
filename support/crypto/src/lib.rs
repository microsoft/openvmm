// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-agnostic cryptographic primitives.
//!
//! This crate abstracts over platform-specific crypto libraries (OpenSSL on
//! Unix, BCrypt on Windows) so that callers never interact with the underlying
//! backend directly.

// UNSAFETY: calling BCrypt APIs
#![cfg_attr(windows, expect(unsafe_code))]

// TODO: Symcrypt somehow
// TODO: Rustcrypto backend for ease of use
// TODO: Windows backends

#[cfg(target_os = "linux")]
pub mod aes_256_cbc;
#[cfg(any(windows, target_os = "linux"))]
pub mod aes_256_gcm;
#[cfg(target_os = "linux")]
pub mod aes_key_wrap;
#[cfg(target_os = "linux")]
pub mod hmac_sha_256;
#[cfg(target_os = "linux")]
pub mod kdf;
#[cfg(target_os = "linux")]
pub mod pkcs7;
#[cfg(target_os = "linux")]
pub mod rsa;
#[cfg(target_os = "linux")]
pub mod sha_256;
#[cfg(target_os = "linux")]
pub mod x509;
#[cfg(any(windows, target_os = "linux"))]
pub mod xts_aes_256;

pub(crate) mod win;

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, thiserror::Error)]
#[error("openssl error during {1}")]
pub struct BackendError(#[source] openssl::error::ErrorStack, &'static str);

/// An error that occurred in the crypto backend, with a description of the
/// operation being performed when the error occurred.
#[cfg(windows)]
#[derive(Clone, Debug, thiserror::Error)]
#[error("bcrypt error during {1}")]
pub struct BackendError(#[source] windows_result::Error, &'static str);
