// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using Windows BCrypt.

use super::EcdsaCurve;
use super::EcdsaError;
use crate::win::AlgHandle;
use crate::win::KeyHandle;
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

/// Return the key length in bits of an imported EC public key, used to
/// determine its curve. Querying the `BCRYPT_KEY_LENGTH` property reads the
/// value directly instead of exporting the whole key blob just to inspect a
/// header field.
fn key_length_bits(handle: &KeyHandle) -> Result<u32, EcdsaError> {
    let mut value = [0u8; size_of::<u32>()];
    let mut written: u32 = 0;
    // SAFETY: FFI call querying a fixed-size `u32` property into a local buffer.
    unsafe {
        BCryptGetProperty(
            BCRYPT_HANDLE(handle.0.0),
            BCRYPT_KEY_LENGTH,
            Some(&mut value),
            &mut written,
            0,
        )
    }
    .ok()
    .map_err(|e| err(e, "BCryptGetProperty(BCRYPT_KEY_LENGTH)"))?;
    Ok(u32::from_ne_bytes(value))
}

#[repr(C)] // Needed for the transmute in as_pub.
pub struct EcdsaKeyPairInner {
    handle: KeyHandle,
    curve: EcdsaCurve,
}

impl EcdsaKeyPairInner {
    pub fn generate(curve: EcdsaCurve) -> Result<Self, EcdsaError> {
        let alg = alg_handle(curve)?;
        let bits = (curve.key_size() * 8) as u32;

        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: FFI call to generate key pair with a valid algorithm handle.
        unsafe { BCryptGenerateKeyPair(alg.0, &mut handle, bits, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptGenerateKeyPair"))?;
        // The handle is now owned; `KeyHandle` destroys it on drop, including
        // if finalization below fails.
        let handle = KeyHandle(handle);

        // SAFETY: FFI call to finalize key pair with a valid handle.
        unsafe { BCryptFinalizeKeyPair(handle.0, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptFinalizeKeyPair"))?;

        Ok(Self { handle, curve })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<Vec<u8>, EcdsaError> {
        let sig_size = self.curve.key_size() * 2;
        let mut signature = vec![0u8; sig_size];
        let mut bytes_written: u32 = 0;

        // SAFETY: FFI call with valid handle and correctly sized buffers.
        unsafe {
            BCryptSignHash(
                self.handle.0,
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

    pub(crate) fn as_pub(&self) -> &EcdsaPublicKeyInner {
        // SAFETY: both types have the same layout, and a private key handle is
        // valid for any public-key operation.
        unsafe { std::mem::transmute::<&EcdsaKeyPairInner, &EcdsaPublicKeyInner>(self) }
    }
}

/// In-memory `BCRYPT_ECCKEY_BLOB` for an ECDSA P-384 public key: the
/// `{ dwMagic, cbKey }` header followed by the `Qx || Qy` affine coordinates.
#[repr(C)] // Needed for the transmute in as_pub.
pub struct EcdsaPublicKeyInner {
    handle: KeyHandle,
    curve: EcdsaCurve,
}

impl EcdsaPublicKeyInner {
    pub fn new(curve: EcdsaCurve, public_key: &[u8]) -> Result<Self, EcdsaError> {
        let alg = alg_handle(curve)?;
        let key_size = curve.key_size();

        // A `BCRYPT_ECCPUBLIC_BLOB` is a `BCRYPT_ECCKEY_BLOB` header followed by
        // the `Qx || Qy` affine coordinates. `BCryptImportKeyPair` reads exactly
        // `2 * cbKey` coordinate bytes and silently ignores any trailing bytes,
        // so it does not reject an over-long `public_key` on its own. Enforce
        // the exact `Qx || Qy` length here so all backends reject non-canonical
        // encodings identically.
        if public_key.len() != key_size * 2 {
            return Err(err(
                windows::core::Error::new(
                    windows::Win32::Foundation::E_INVALIDARG,
                    "ECDSA public key is not the expected length (Qx || Qy)",
                ),
                "validating ECDSA public key length",
            ));
        }

        let header = BCRYPT_ECCKEY_BLOB {
            dwMagic: match curve {
                EcdsaCurve::P384 => BCRYPT_ECDSA_PUBLIC_P384_MAGIC,
            },
            cbKey: key_size as u32,
        };
        let mut blob = Vec::with_capacity(size_of::<BCRYPT_ECCKEY_BLOB>() + public_key.len());
        // SAFETY: `BCRYPT_ECCKEY_BLOB` is a `repr(C)` plain-old-data struct of
        // two `u32`s, so reading its bytes is well-defined.
        blob.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&header).cast::<u8>(),
                size_of::<BCRYPT_ECCKEY_BLOB>(),
            )
        });
        blob.extend_from_slice(public_key);

        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: FFI import with a valid algorithm handle and a public key blob.
        unsafe { BCryptImportKeyPair(alg.0, None, BCRYPT_ECCPUBLIC_BLOB, &mut handle, &blob, 0) }
            .ok()
            .map_err(|e| err(e, "BCryptImportKeyPair"))?;

        Ok(Self {
            handle: KeyHandle(handle),
            curve,
        })
    }

    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: spki_der.len() as u32,
            pbData: spki_der.as_ptr().cast_mut(),
        };
        let decoded =
            crate::x509::win::decode_object::<CERT_PUBLIC_KEY_INFO>(X509_PUBLIC_KEY_INFO, &blob)
                .map_err(|e| EcdsaError(crate::x509::win::backend_err(e)))?;
        Self::from_cert_public_key_info(decoded.get())
    }

    /// Import a `CERT_PUBLIC_KEY_INFO` (SubjectPublicKeyInfo) as an ECDSA public
    /// key, rejecting non-EC keys and keys on unsupported curves. The curve is
    /// determined from the imported key. This lets the `x509` module hand a
    /// certificate's already-parsed public-key info directly to the ECDSA
    /// backend rather than re-serializing it.
    pub(crate) fn from_cert_public_key_info(
        info: &CERT_PUBLIC_KEY_INFO,
    ) -> Result<Self, EcdsaError> {
        // Reject non-EC keys. The OID is a null-terminated ASCII PSTR.
        let oid = info.Algorithm.pszObjId;
        // SAFETY: `info` is a valid CERT_PUBLIC_KEY_INFO; OID strings produced
        // by crypt32 and the szOID_* constants are null-terminated ASCII.
        let is_ec = !oid.is_null() && unsafe { oid.as_bytes() == szOID_ECC_PUBLIC_KEY.as_bytes() };
        if !is_ec {
            return Err(err(
                windows::core::Error::new(
                    windows::Win32::Foundation::E_INVALIDARG,
                    "public key is not an ECDSA key",
                ),
                "validating public key algorithm",
            ));
        }

        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: `info` is a valid CERT_PUBLIC_KEY_INFO. `CryptImportPublicKeyInfoEx2`
        // infers the curve from the encoded key.
        unsafe {
            CryptImportPublicKeyInfoEx2(
                X509_ASN_ENCODING,
                info,
                CRYPT_IMPORT_PUBLIC_KEY_FLAGS(0),
                None,
                &mut handle,
            )
        }
        .map_err(|e| err(e, "CryptImportPublicKeyInfoEx2"))?;
        let handle = KeyHandle(handle);

        // Determine the curve from the imported key. NIST prime curves each
        // have a unique field size in bits, so the key length identifies the
        // curve (P-384 => 384).
        let curve = match key_length_bits(&handle)? {
            384 => EcdsaCurve::P384,
            _ => {
                return Err(err(
                    windows::core::Error::new(
                        windows::Win32::Foundation::E_INVALIDARG,
                        "unsupported or unrecognized EC curve",
                    ),
                    "determining public key curve",
                ));
            }
        };

        Ok(Self { handle, curve })
    }

    pub fn verify_prehash(&self, hash: &[u8], signature: &[u8]) -> Result<bool, EcdsaError> {
        // A signature must be exactly `r || s`, each `curve.key_size()` bytes.
        if signature.len() != self.curve.key_size() * 2 {
            return Ok(false);
        }

        // SAFETY: FFI call with a valid key handle and valid input slices.
        let status =
            unsafe { BCryptVerifySignature(self.handle.0, None, hash, signature, BCRYPT_FLAGS(0)) };

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
                self.handle.0,
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
                self.handle.0,
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
