// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using Windows BCrypt.

use super::EcdsaCurve;
use super::EcdsaError;
use crate::win::AlgHandle;
use std::sync::LazyLock;
use windows::Win32::Foundation::STATUS_INVALID_SIGNATURE;
use windows::Win32::Security::Cryptography::*;

static ECDSA_P384: LazyLock<Result<AlgHandle, EcdsaError>> = LazyLock::new(|| {
    let mut handle = BCRYPT_ALG_HANDLE::default();
    // SAFETY: errors are handled before the handle is used; the handle is
    // closed on drop via `AlgHandle`.
    unsafe {
        BCryptOpenAlgorithmProvider(
            &mut handle,
            BCRYPT_ECDSA_P384_ALGORITHM,
            None,
            BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS(0),
        )
    }
    .ok()
    .map(|()| AlgHandle(handle))
    .map_err(|e| err(e, "BCryptOpenAlgorithmProvider"))
});

fn err(e: windows_result::Error, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError(e, op))
}

fn alg_handle(curve: EcdsaCurve) -> Result<&'static AlgHandle, EcdsaError> {
    match curve {
        EcdsaCurve::P384 => ECDSA_P384.as_ref().map_err(|e| EcdsaError(e.0.clone())),
    }
}

pub struct EcdsaKeyPairInner {
    handle: BCRYPT_KEY_HANDLE,
    curve: EcdsaCurve,
}

impl Drop for EcdsaKeyPairInner {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: handle is valid and owned by this struct.
            let _ = unsafe { BCryptDestroyKey(self.handle) };
        }
    }
}

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        let alg = alg_handle(curve)?;
        let bits = (curve.key_size() * 8) as u32;

        let mut key = BCRYPT_KEY_HANDLE::default();
        // SAFETY: FFI call to generate key pair with a valid algorithm handle.
        unsafe { BCryptGenerateKeyPair(alg.0, &mut key, bits, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptGenerateKeyPair"))?;

        // SAFETY: FFI call to finalize key pair.
        unsafe { BCryptFinalizeKeyPair(key, 0) }.ok().map_err(|e| {
            // SAFETY: key was successfully generated and must be destroyed on error.
            let _ = unsafe { BCryptDestroyKey(key) };
            err(e, "BCryptFinalizeKeyPair")
        })?;

        Ok(Self { handle: key, curve })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        let sig_size = self.curve.key_size() * 2;
        let mut signature = vec![0u8; sig_size];
        let mut bytes_written: u32 = 0;

        // SAFETY: FFI call with valid handle and correctly sized buffers.
        unsafe {
            BCryptSignHash(
                self.handle,
                None,
                hash,
                Some(&mut signature),
                &mut bytes_written,
                BCRYPT_FLAGS(0),
            )
        }
        .ok()
        .map_err(|e| err(e, "BCryptSignHash"))?;

        signature.truncate(bytes_written as usize);
        Ok(signature)
    }

    pub fn verify_prehash(&self, hash: &[u8], signature: &[u8]) -> Result<bool, EcdsaError> {
        // A signature must be exactly `r || s`, each `curve.key_size()` bytes.
        if signature.len() != self.curve.key_size() * 2 {
            return Ok(false);
        }

        // SAFETY: FFI call with a valid key handle and valid input slices.
        let status =
            unsafe { BCryptVerifySignature(self.handle, None, hash, signature, BCRYPT_FLAGS(0)) };

        // A signature that simply does not match yields STATUS_INVALID_SIGNATURE,
        // which is a valid "not verified" result rather than an operational error.
        if status == STATUS_INVALID_SIGNATURE {
            return Ok(false);
        }
        status.ok().map_err(|e| err(e, "BCryptVerifySignature"))?;
        Ok(true)
    }

    pub fn public_key_bytes(&self) -> Result<Vec<u8>, EcdsaError> {
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
        let header_size = size_of::<BCRYPT_ECCKEY_BLOB>();
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
        Ok(blob[header_size..header_size + key_size * 2].to_vec())
    }
}
