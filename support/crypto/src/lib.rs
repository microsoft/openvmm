// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backend-agnostic cryptographic primitives.
//!
//! This crate abstracts over platform-specific crypto libraries (OpenSSL on
//! Unix, BCrypt on Windows) so that callers never interact with the underlying
//! backend directly.
//!
//! Symmetric cipher modules ([`aes_256_gcm`], [`xts_aes_256`]) are available
//! when the `ossl` or `win` feature is enabled. Additional operations such as
//! RSA, X.509, PKCS#7, HMAC, SHA-256, AES-CBC, AES key wrap, and KDF are
//! gated behind the `ossl_crypto` feature (requires OpenSSL).

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
