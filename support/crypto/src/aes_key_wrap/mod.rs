// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES key wrap with padding (RFC 5649).

#[cfg(unix)]
mod ossl;
#[cfg(unix)]
use ossl as sys;

use thiserror::Error;

/// An error for AES key wrap operations.
#[derive(Clone, Debug, Error)]
pub enum AesKeyWrapError {
    /// The wrapping key size is not 16, 24, or 32 bytes.
    #[error("invalid wrapping key size {0}")]
    InvalidKeySize(usize),
    /// A backend cryptographic error occurred.
    #[error("AES key wrap error")]
    Backend(#[source] super::BackendError),
}

/// AES key wrap with padding (RFC 5649).
///
/// The wrapping key must be 16, 24, or 32 bytes (128, 192, or 256 bits).
pub struct AesKeyWrap(sys::AesKeyWrapInner);

impl AesKeyWrap {
    /// Creates a new AES key wrap context.
    ///
    /// `key` must be 16, 24, or 32 bytes.
    pub fn new(key: &[u8]) -> Result<Self, AesKeyWrapError> {
        sys::AesKeyWrapInner::new(key).map(Self)
    }

    /// Returns a context for wrapping keys.
    pub fn wrapper(&self) -> Result<AesKeyWrapCtx<'_>, AesKeyWrapError> {
        Ok(AesKeyWrapCtx(self.0.wrap_ctx()?))
    }

    /// Returns a context for unwrapping keys.
    pub fn unwrapper(&self) -> Result<AesKeyUnwrapCtx<'_>, AesKeyWrapError> {
        Ok(AesKeyUnwrapCtx(self.0.unwrap_ctx()?))
    }
}

/// Context for AES key wrapping.
pub struct AesKeyWrapCtx<'a>(sys::AesKeyWrapCtxInner<'a>);

impl AesKeyWrapCtx<'_> {
    /// Wraps `payload` using AES key wrap with padding (RFC 5649).
    pub fn wrap(&mut self, payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        self.0.wrap(payload)
    }
}

/// Context for AES key unwrapping.
pub struct AesKeyUnwrapCtx<'a>(sys::AesKeyUnwrapCtxInner<'a>);

impl AesKeyUnwrapCtx<'_> {
    /// Unwraps `wrapped_payload` using AES key unwrap with padding (RFC 5649).
    pub fn unwrap(&mut self, wrapped_payload: &[u8]) -> Result<Vec<u8>, AesKeyWrapError> {
        self.0.unwrap(wrapped_payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_key_wrap_with_padding_kat() {
        const KEK: [u8; 24] = [
            0x58, 0x40, 0xdf, 0x6e, 0x29, 0xb0, 0x2a, 0xf1, 0xab, 0x49, 0x3b, 0x70, 0x5b, 0xf1,
            0x6e, 0xa1, 0xae, 0x83, 0x38, 0xf4, 0xdc, 0xc1, 0x76, 0xa8,
        ];
        const KEY20: [u8; 20] = [
            0xc3, 0x7b, 0x7e, 0x64, 0x92, 0x58, 0x43, 0x40, 0xbe, 0xd1, 0x22, 0x07, 0x80, 0x89,
            0x41, 0x15, 0x50, 0x68, 0xf7, 0x38,
        ];
        const WRAP20: [u8; 32] = [
            0x13, 0x8b, 0xde, 0xaa, 0x9b, 0x8f, 0xa7, 0xfc, 0x61, 0xf9, 0x77, 0x42, 0xe7, 0x22,
            0x48, 0xee, 0x5a, 0xe6, 0xae, 0x53, 0x60, 0xd1, 0xae, 0x6a, 0x5f, 0x54, 0xf3, 0x73,
            0xfa, 0x54, 0x3b, 0x6a,
        ];
        const KEY7: [u8; 7] = [0x46, 0x6f, 0x72, 0x50, 0x61, 0x73, 0x69];
        const WRAP7: [u8; 16] = [
            0xaf, 0xbe, 0xb0, 0xf0, 0x7d, 0xfb, 0xf5, 0x41, 0x92, 0x00, 0xf2, 0xcc, 0xb5, 0x0b,
            0xb2, 0x4f,
        ];

        let kek = AesKeyWrap::new(&KEK).unwrap();

        let wrapped_key = kek.wrapper().unwrap().wrap(&KEY20).unwrap();
        assert_eq!(wrapped_key, WRAP20);

        let unwrapped_key = kek.unwrapper().unwrap().unwrap(&WRAP20).unwrap();
        assert_eq!(unwrapped_key, KEY20);

        let wrapped_key = kek.wrapper().unwrap().wrap(&KEY7).unwrap();
        assert_eq!(wrapped_key, WRAP7);

        let unwrapped_key = kek.unwrapper().unwrap().unwrap(&WRAP7).unwrap();
        assert_eq!(unwrapped_key, KEY7);
    }

    #[test]
    fn aes_key_wrap_with_padding_roundtrip() {
        const KEY: [u8; 32] = [
            0x3f, 0xf4, 0xdb, 0xdb, 0x74, 0xd9, 0x3d, 0x22, 0x35, 0xc6, 0x7c, 0x9e, 0x17, 0x6a,
            0x88, 0x7f, 0xf9, 0x11, 0xd6, 0x5b, 0x5a, 0x56, 0x06, 0xa7, 0xfb, 0x52, 0x58, 0xfc,
            0x4e, 0x76, 0xce, 0x49,
        ];

        const AES_WRAPPED_KEY: [u8; 40] = [
            0x56, 0x53, 0xe9, 0x29, 0xa9, 0x35, 0x0c, 0x32, 0xd0, 0x24, 0x22, 0xb4, 0x98, 0xe1,
            0x13, 0xe7, 0x4a, 0x81, 0xc1, 0xf3, 0xb2, 0xa6, 0x27, 0x70, 0x6e, 0x0d, 0x12, 0x97,
            0xfd, 0xa5, 0x07, 0x0a, 0x5e, 0xb0, 0xd2, 0xde, 0xb2, 0x8a, 0x06, 0x72,
        ];

        const WRAPPING_KEY: [u8; 32] = [
            0x10, 0x84, 0xD2, 0x2F, 0x53, 0x5F, 0xD3, 0x10, 0xE2, 0xC6, 0x17, 0x31, 0x3D, 0xCA,
            0xE7, 0xEF, 0x19, 0xDD, 0x45, 0x2A, 0xED, 0x1C, 0xE6, 0xB1, 0xBE, 0xF5, 0xB9, 0xD0,
            0x1B, 0xF1, 0x5F, 0x44,
        ];

        let kw = AesKeyWrap::new(&WRAPPING_KEY).unwrap();

        let wrapped_key = kw.wrapper().unwrap().wrap(&KEY).unwrap();
        assert_eq!(wrapped_key, AES_WRAPPED_KEY);

        let unwrapped_key = kw.unwrapper().unwrap().unwrap(&AES_WRAPPED_KEY).unwrap();
        assert_eq!(unwrapped_key, KEY);
    }
}
