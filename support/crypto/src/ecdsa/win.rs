// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using Windows BCrypt.

use super::EcdsaCurve;
use super::EcdsaError;

fn err(e: windows::core::Error, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError(e, op))
}

pub struct EcdsaKeyPairInner {
    handle: windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE,
    curve: EcdsaCurve,
}

impl Drop for EcdsaKeyPairInner {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle is valid and owned by this struct.
            let _ = unsafe {
                windows::Win32::Security::Cryptography::BCryptDestroyKey(self.handle)
            };
        }
    }
}

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        use windows::Win32::Security::Cryptography::*;

        let alg_id = match curve {
            EcdsaCurve::P384 => BCRYPT_ECDSA_P384_ALGORITHM,
        };
        let bits: u32 = match curve {
            EcdsaCurve::P384 => 384,
        };

        let mut alg = BCRYPT_ALG_HANDLE::default();
        // SAFETY: FFI call to open algorithm provider.
        unsafe { BCryptOpenAlgorithmProvider(&mut alg, alg_id, None, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptOpenAlgorithmProvider"))?;

        // Ensure the algorithm handle is closed on all paths.
        struct AlgGuard(BCRYPT_ALG_HANDLE);
        impl Drop for AlgGuard {
            fn drop(&mut self) {
                // SAFETY: handle was successfully opened.
                let _ = unsafe { BCryptCloseAlgorithmProvider(self.0, 0) };
            }
        }
        let _alg_guard = AlgGuard(alg);

        let mut key = BCRYPT_KEY_HANDLE::default();
        // SAFETY: FFI call to generate key pair.
        unsafe { BCryptGenerateKeyPair(alg, &mut key, bits, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptGenerateKeyPair"))?;

        // SAFETY: FFI call to finalize key pair.
        unsafe { BCryptFinalizeKeyPair(key, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptFinalizeKeyPair"))?;

        Ok(Self { handle: key, curve })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        use windows::Win32::Security::Cryptography::*;

        let sig_size = self.curve.key_size() * 2;
        let mut signature = vec![0u8; sig_size];
        let mut bytes_written: u32 = 0;

        // SAFETY: FFI call with valid handle and correctly sized buffers.
        unsafe {
            BCryptSignHash(
                self.handle,
                None,
                Some(hash),
                Some(&mut signature),
                &mut bytes_written,
                0,
            )
        }
        .ok()
        .map_err(|e| err(e, "BCryptSignHash"))?;

        signature.truncate(bytes_written as usize);
        Ok(signature)
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
        use windows::Win32::Security::Cryptography::*;

        let mut blob_len: u32 = 0;
        // SAFETY: FFI call to query the required buffer size.
        unsafe {
            BCryptExportKey(
                self.handle,
                None,
                BCRYPT_ECCPUBLIC_BLOB,
                None,
                &mut blob_len,
                0,
            )
        }
        .ok()
        .map_err(|e| err(e, "BCryptExportKey(size)"))?;

        let mut blob = vec![0u8; blob_len as usize];
        // SAFETY: FFI call to export the key with correctly sized buffer.
        unsafe {
            BCryptExportKey(
                self.handle,
                None,
                BCRYPT_ECCPUBLIC_BLOB,
                Some(&mut blob),
                &mut blob_len,
                0,
            )
        }
        .ok()
        .map_err(|e| err(e, "BCryptExportKey(data)"))?;

        // BCrypt ECC public blob layout: BCRYPT_ECCKEY_BLOB header + X + Y
        let header_size = std::mem::size_of::<BCRYPT_ECCKEY_BLOB>();
        let key_size = self.curve.key_size();

        if (blob_len as usize) < header_size + key_size * 2 {
            return Err(err(
                windows::core::Error::new(
                    windows::Win32::Foundation::E_UNEXPECTED,
                    "public key blob too small",
                ),
                "validating public key blob size",
            ));
        }

        // Return just Qx || Qy (skip the BCRYPT_ECCKEY_BLOB header).
        let result = blob[header_size..header_size + key_size * 2].to_vec();
        Ok(result)
    }
}
