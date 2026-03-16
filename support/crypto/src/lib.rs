// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-agnostic cryptographic primitives.
//!
//! This crate abstracts over platform-specific crypto libraries (OpenSSL on
//! Unix, BCrypt on Windows) so that callers never interact with the underlying
//! backend directly.

// TODO: Symcrypt somehow
// TODO: Rustcrypto backend for ease of use
// TODO: delete block_crypto

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

use thiserror::Error;

#[cfg(unix)]
#[derive(Debug, Error)]
#[error("openssl error during {1}")]
struct BackendError(#[source] openssl::error::ErrorStack, &'static str);

#[cfg(windows)]
#[derive(Debug, Error)]
#[error("bcrypt error during {1}")]
struct BackendError(#[source] windows_result::Error, &'static str);
