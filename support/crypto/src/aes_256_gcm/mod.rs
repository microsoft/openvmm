// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! AES-256-GCM encryption and decryption.

#![cfg(any(openssl, symcrypt, all(native, windows)))]

cfg_select! {
    openssl => {
        mod ossl;
        use ossl as sys;
    }
    all(native, windows) => {
        mod win;
        use win as sys;
    }
    symcrypt => {
        mod symcrypt;
        use symcrypt as sys;
    }
}

use thiserror::Error;

/// The required key length for the algorithm.
///
/// An AES-256-GCM key is 256 bits.
pub const KEY_LEN: usize = 32;

/// The required IV length for the algorithm.
///
/// An AES-256-GCM IV is 96 bits (12 bytes).
pub const IV_LEN: usize = 12;

/// AES-256-GCM encryption/decryption.
pub struct Aes256Gcm(sys::Aes256GcmInner);

/// An error for AES-256-GCM cryptographic operations.
#[derive(Clone, Debug, Error)]
#[error("AES-256-GCM operation failed")]
pub struct Aes256GcmError(#[source] super::BackendError);

impl Aes256Gcm {
    /// Creates a new AES-256-GCM encryption/decryption context.
    pub fn new(key: &[u8; KEY_LEN]) -> Result<Self, Aes256GcmError> {
        sys::Aes256GcmInner::new(key).map(Self)
    }

    /// Returns a context for encrypting data.
    pub fn encrypt(&self) -> Result<Aes256GcmEncCtx<'_>, Aes256GcmError> {
        Ok(Aes256GcmEncCtx(self.0.enc_ctx()?))
    }

    /// Returns a context for decrypting data.
    pub fn decrypt(&self) -> Result<Aes256GcmDecCtx<'_>, Aes256GcmError> {
        Ok(Aes256GcmDecCtx(self.0.dec_ctx()?))
    }
}

/// Context for AES-256-GCM encryption.
pub struct Aes256GcmEncCtx<'a>(sys::Aes256GcmEncCtxInner<'a>);

impl Aes256GcmEncCtx<'_> {
    /// Encrypts `data` using the provided `iv` and produces the
    /// authentication tag in `tag`.
    pub fn cipher(
        &mut self,
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &mut [u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        self.0.cipher(iv, data, tag)
    }
}

/// Context for AES-256-GCM decryption.
pub struct Aes256GcmDecCtx<'a>(sys::Aes256GcmDecCtxInner<'a>);

impl Aes256GcmDecCtx<'_> {
    /// Decrypts `data` using the provided `iv` and verifies the
    /// authentication `tag`.
    pub fn cipher(
        &mut self,
        iv: &[u8; IV_LEN],
        data: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, Aes256GcmError> {
        self.0.cipher(iv, data, tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aes_256_gcm() {
        // Test Case 14 from the NIST GCM specification (gcm-spec.pdf).
        let key = hex::decode("feffe9928665731c6d6a8f9467308308feffe9928665731c6d6a8f9467308308")
            .unwrap();
        let iv = hex::decode("cafebabefacedbaddecaf888")
            .unwrap()
            .try_into()
            .unwrap();
        let plain = hex::decode(
            "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
             1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
        )
        .unwrap();
        let cipher = hex::decode(
            "522dc1f099567d07f47f37a32a84427d643a8cdcbfe5c0c97598a2bd2555d1aa\
             8cb08e48590dbb3da7b08b1056828838c5f61e6393ba7a0abcc9f662898015ad",
        )
        .unwrap();
        let tag = hex::decode("b094dac5d93471bdec1a502270e3cc6c").unwrap();

        let aes = Aes256Gcm::new(&key.try_into().unwrap()).unwrap();
        let mut enc_ctx = aes.encrypt().unwrap();
        let mut enc_tag = vec![0u8; tag.len()];
        let enc_cipher = enc_ctx.cipher(&iv, &plain, &mut enc_tag).unwrap();
        assert_eq!(enc_cipher, cipher);
        assert_eq!(enc_tag, tag);

        let mut dec_ctx = aes.decrypt().unwrap();
        let dec_plain = dec_ctx.cipher(&iv, &cipher, &tag).unwrap();
        assert_eq!(dec_plain, plain);
    }
}

#[cfg(test)]
mod authentication_contract {
    use super::*;
    use test_with_tracing::test;

    // Test: Reuse cipher contexts across failed authentication and message boundaries.
    // Expected: Failure releases no plaintext and does not poison the next operation.
    #[test]
    fn reused_context_recovers_after_authentication_failure() {
        let aes = Aes256Gcm::new(&[0x41; 32]).unwrap();
        let mut encrypt = aes.encrypt().unwrap();
        let mut decrypt = aes.decrypt().unwrap();
        for (index, plain) in [vec![0x19; 17], vec![], vec![0x83; 65]]
            .into_iter()
            .enumerate()
        {
            let mut iv = [0x73; 12];
            iv[11] = index as u8;
            let mut tag = [0; 16];
            let ciphertext = encrypt.cipher(&iv, &plain, &mut tag).unwrap();
            let mut bad_tag = tag;
            bad_tag[0] ^= 1;
            assert!(decrypt.cipher(&iv, &ciphertext, &bad_tag).is_err());
            assert_eq!(decrypt.cipher(&iv, &ciphertext, &tag).unwrap(), plain);
        }
    }

    // Test: Authenticate empty, partial-block, aligned and multi-block messages.
    // Expected: Original ciphertext decrypts exactly; modified tags, nonces,
    //       keys or ciphertext return errors instead of unauthenticated bytes.
    #[test]
    fn authenticated_decryption_rejects_modified_inputs() {
        let aes = Aes256Gcm::new(&[0x41; 32]).unwrap();
        let wrong_key = Aes256Gcm::new(&[0x42; 32]).unwrap();
        for (index, length) in [0, 1, 15, 16, 17, 257].into_iter().enumerate() {
            let mut iv = [0x93; 12];
            iv[11] = index as u8;
            let plain: Vec<_> = (0..length).map(|i| (i * 37 + 13) as u8).collect();
            let mut tag = [0; 16];
            let ciphertext = aes
                .encrypt()
                .unwrap()
                .cipher(&iv, &plain, &mut tag)
                .unwrap();
            assert_eq!(
                aes.decrypt()
                    .unwrap()
                    .cipher(&iv, &ciphertext, &tag)
                    .unwrap(),
                plain
            );
            assert!(
                wrong_key
                    .decrypt()
                    .unwrap()
                    .cipher(&iv, &ciphertext, &tag)
                    .is_err()
            );
            let mut altered_iv = iv;
            altered_iv[0] ^= 0x80;
            assert!(
                aes.decrypt()
                    .unwrap()
                    .cipher(&altered_iv, &ciphertext, &tag)
                    .is_err()
            );
            for byte in [0, 15] {
                let mut altered_tag = tag;
                altered_tag[byte] ^= 1;
                assert!(
                    aes.decrypt()
                        .unwrap()
                        .cipher(&iv, &ciphertext, &altered_tag)
                        .is_err()
                );
            }
            if !ciphertext.is_empty() {
                let mut altered = ciphertext.clone();
                altered[length / 2] ^= 1;
                assert!(aes.decrypt().unwrap().cipher(&iv, &altered, &tag).is_err());
            }
        }
    }
}
