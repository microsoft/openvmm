// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649).

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// Error returned by AES key wrap operations.
#[derive(Debug, Error)]
#[error("AES key wrap error")]
pub struct AesKeyWrapError(#[source] super::BackendError);

/// Wrap `payload` using AES key wrap with padding (RFC 5649).
pub fn wrap(wrapping_key: &[u8], payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
    sys::wrap(wrapping_key, payload)
}

/// Unwrap `wrapped_payload` using AES key unwrap with padding (RFC 5649).
pub fn unwrap(unwrapping_key: &[u8], wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
    sys::unwrap(unwrapping_key, wrapped_payload)
}
