// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ECDSA implementation using Windows BCrypt.

use super::EcdsaCurve;
use super::EcdsaError;
use super::EcdsaPublicKeyComponents;
use super::EcdsaSignatureComponents;
use crate::win::AlgHandle;
use crate::win::KeyHandle;
use crate::win::PublicKeyInfo;
use crate::win::decode_object;
use crate::win::encode_object;
use std::sync::LazyLock;
use windows::Win32::Foundation::STATUS_INVALID_SIGNATURE;
use windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_ECCKEY_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_ECCPUBLIC_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_ECDSA_P384_ALGORITHM;
use windows::Win32::Security::Cryptography::BCRYPT_ECDSA_PUBLIC_P384_MAGIC;
use windows::Win32::Security::Cryptography::BCRYPT_FLAGS;
use windows::Win32::Security::Cryptography::BCRYPT_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_LENGTH;
use windows::Win32::Security::Cryptography::BCRYPT_OPEN_ALGORITHM_PROVIDER_FLAGS;
use windows::Win32::Security::Cryptography::BCryptExportKey;
use windows::Win32::Security::Cryptography::BCryptFinalizeKeyPair;
use windows::Win32::Security::Cryptography::BCryptGenerateKeyPair;
use windows::Win32::Security::Cryptography::BCryptGetProperty;
use windows::Win32::Security::Cryptography::BCryptImportKeyPair;
use windows::Win32::Security::Cryptography::BCryptOpenAlgorithmProvider;
use windows::Win32::Security::Cryptography::BCryptSignHash;
use windows::Win32::Security::Cryptography::BCryptVerifySignature;
use windows::Win32::Security::Cryptography::CERT_ECC_SIGNATURE;
use windows::Win32::Security::Cryptography::CERT_PUBLIC_KEY_INFO;
use windows::Win32::Security::Cryptography::CRYPT_IMPORT_PUBLIC_KEY_FLAGS;
use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows::Win32::Security::Cryptography::CryptImportPublicKeyInfoEx2;
use windows::Win32::Security::Cryptography::X509_ASN_ENCODING;
use windows::Win32::Security::Cryptography::X509_ECC_SIGNATURE;
use windows::Win32::Security::Cryptography::X509_OBJECT_IDENTIFIER;
use windows::Win32::Security::Cryptography::X509_PUBLIC_KEY_INFO;
use windows::Win32::Security::Cryptography::szOID_ECC_CURVE_P384;
use windows::Win32::Security::Cryptography::szOID_ECC_PUBLIC_KEY;

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
    .map_err(|e| err(e, "opening the ECDSA algorithm provider"))
});

fn err(err: windows_result::Error, op: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::Windows(err, op))
}

/// An error for input that decoded but is not valid or supported.
fn invalid_err(reason: &'static str) -> EcdsaError {
    EcdsaError(crate::BackendError::Invalid(reason))
}

/// BCrypt's fixed-width, big-endian `r || s` representation.
pub struct EcdsaSignatureInner(Vec<u8>);

impl EcdsaSignatureInner {
    pub fn from_der(curve: EcdsaCurve, der: &[u8]) -> Result<Self, EcdsaError> {
        let decoded =
            decode_object::<CERT_ECC_SIGNATURE>(X509_ECC_SIGNATURE, der).map_err(EcdsaError)?;
        let components = decoded.get();
        let canonical = encode_object(X509_ECC_SIGNATURE, components)
            .map_err(|e| err(e, "encoding the ECDSA signature as DER"))?;
        if canonical != der {
            return Err(invalid_err("ECDSA signature is not unsigned canonical DER"));
        }

        let size = curve.key_size();
        let mut signature = vec![0; size * 2];
        for (component, output) in [&components.r, &components.s]
            .iter()
            .zip(signature.chunks_exact_mut(size))
        {
            if component.cbData as usize > size {
                return Err(invalid_err(
                    "ECDSA signature component exceeds the curve's key size",
                ));
            }
            if component.cbData == 0 || component.pbData.is_null() {
                return Err(invalid_err("malformed ECDSA signature component"));
            }
            // SAFETY: CryptoAPI populated this blob in the still-owned decoded allocation.
            let bytes =
                unsafe { std::slice::from_raw_parts(component.pbData, component.cbData as usize) };
            // CryptoAPI returns little-endian unsigned integers.
            for (output, byte) in output.iter_mut().rev().zip(bytes) {
                *output = *byte;
            }
        }
        Ok(Self(signature))
    }

    pub fn from_components(
        curve: EcdsaCurve,
        components: &EcdsaSignatureComponents,
    ) -> Result<Self, EcdsaError> {
        if components.r.len() != curve.key_size() || components.s.len() != curve.key_size() {
            return Err(invalid_err("unexpected ECDSA signature component lengths"));
        }
        Ok(Self(
            [components.r.as_slice(), components.s.as_slice()].concat(),
        ))
    }

    pub fn to_components(&self) -> Result<EcdsaSignatureComponents, EcdsaError> {
        let (r, s) = self.0.split_at(self.0.len() / 2);
        Ok(EcdsaSignatureComponents {
            r: r.to_vec(),
            s: s.to_vec(),
        })
    }

    pub fn to_der(&self) -> Result<Vec<u8>, EcdsaError> {
        let (r, s) = self.0.split_at(self.0.len() / 2);
        let mut r: Vec<_> = r.iter().rev().copied().collect();
        let mut s: Vec<_> = s.iter().rev().copied().collect();
        let components = CERT_ECC_SIGNATURE {
            r: CRYPT_INTEGER_BLOB {
                cbData: r.len() as u32,
                pbData: r.as_mut_ptr(),
            },
            s: CRYPT_INTEGER_BLOB {
                cbData: s.len() as u32,
                pbData: s.as_mut_ptr(),
            },
        };
        encode_object(X509_ECC_SIGNATURE, &components)
            .map_err(|e| err(e, "encoding the ECDSA signature as DER"))
    }
}

fn alg_handle(curve: EcdsaCurve) -> Result<&'static AlgHandle, EcdsaError> {
    match curve {
        EcdsaCurve::P384 => ECDSA_P384.as_ref().map_err(Clone::clone),
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
    .map_err(|e| err(e, "querying the EC key length"))?;
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
            .map_err(|e| err(e, "generating the ECDSA key pair"))?;
        // The handle is now owned; `KeyHandle` destroys it on drop, including
        // if finalization below fails.
        let handle = KeyHandle(handle);

        // SAFETY: FFI call to finalize key pair with a valid handle.
        unsafe { BCryptFinalizeKeyPair(handle.0, 0) }
            .ok()
            .map_err(|e| err(e, "finalizing the ECDSA key pair"))?;

        Ok(Self { handle, curve })
    }

    pub fn sign_prehash(&self, hash: &[u8]) -> Result<EcdsaSignatureInner, EcdsaError> {
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
        .map_err(|e| err(e, "signing the message hash"))?;

        if bytes_written as usize != sig_size {
            return Err(invalid_err("unexpected ECDSA signature length"));
        }
        Ok(EcdsaSignatureInner(signature))
    }

    pub(crate) fn as_pub(&self) -> &EcdsaPublicKeyInner {
        // SAFETY: both types have the same layout, and a private key handle is
        // valid for any public-key operation.
        unsafe { std::mem::transmute::<&EcdsaKeyPairInner, &EcdsaPublicKeyInner>(self) }
    }
}

#[repr(C)] // Needed for the transmute in as_pub.
pub struct EcdsaPublicKeyInner {
    handle: KeyHandle,
    curve: EcdsaCurve,
}

impl EcdsaPublicKeyInner {
    pub fn from_public_key_bytes(
        curve: EcdsaCurve,
        public_key: &EcdsaPublicKeyComponents,
    ) -> Result<Self, EcdsaError> {
        let alg = alg_handle(curve)?;
        let key_size = curve.key_size();

        if public_key.qx.len() != key_size || public_key.qy.len() != key_size {
            return Err(invalid_err("unexpected EC public key coordinate lengths"));
        }

        let header = BCRYPT_ECCKEY_BLOB {
            dwMagic: match curve {
                EcdsaCurve::P384 => BCRYPT_ECDSA_PUBLIC_P384_MAGIC,
            },
            cbKey: key_size as u32,
        };
        let mut blob = Vec::with_capacity(size_of::<BCRYPT_ECCKEY_BLOB>() + key_size * 2);
        // SAFETY: `BCRYPT_ECCKEY_BLOB` is a `repr(C)` plain-old-data struct of
        // two `u32`s, so reading its bytes is well-defined.
        blob.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&header).cast::<u8>(),
                size_of::<BCRYPT_ECCKEY_BLOB>(),
            )
        });
        blob.extend_from_slice(&public_key.qx);
        blob.extend_from_slice(&public_key.qy);

        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: FFI import with a valid algorithm handle and a public key blob.
        unsafe { BCryptImportKeyPair(alg.0, None, BCRYPT_ECCPUBLIC_BLOB, &mut handle, &blob, 0) }
            .ok()
            .map_err(|e| err(e, "importing the ECDSA public key"))?;

        Ok(Self {
            handle: KeyHandle(handle),
            curve,
        })
    }

    pub fn from_public_key_der(spki_der: &[u8]) -> Result<Self, EcdsaError> {
        let decoded = decode_object::<CERT_PUBLIC_KEY_INFO>(X509_PUBLIC_KEY_INFO, spki_der)
            .map_err(EcdsaError)?;
        Self::from_cert_public_key_info(decoded.get())
    }

    pub fn to_public_key_der(&self) -> Result<Vec<u8>, EcdsaError> {
        let info = self.to_public_key_info()?;
        encode_object(X509_PUBLIC_KEY_INFO, &info.as_info())
            .map_err(|e| err(e, "encoding the SubjectPublicKeyInfo"))
    }

    pub(crate) fn to_public_key_info(&self) -> Result<PublicKeyInfo, EcdsaError> {
        let point = self.public_key_bytes()?.to_uncompressed_point();
        let curve_oid = match self.curve {
            EcdsaCurve::P384 => szOID_ECC_CURVE_P384,
        };
        let parameters = encode_object(X509_OBJECT_IDENTIFIER, &curve_oid)
            .map_err(|e| err(e, "encoding the EC named-curve OID"))?;
        Ok(PublicKeyInfo::new(szOID_ECC_PUBLIC_KEY, parameters, point))
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
            return Err(invalid_err("public key is not an EC key"));
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
        .map_err(|e| err(e, "importing the certificate public key"))?;
        let handle = KeyHandle(handle);

        // Determine the curve from the imported key. NIST prime curves each
        // have a unique field size in bits, so the key length identifies the
        // curve (P-384 => 384).
        let curve = match key_length_bits(&handle)? {
            384 => EcdsaCurve::P384,
            _ => {
                return Err(invalid_err("public key uses an unsupported EC curve"));
            }
        };

        Ok(Self { handle, curve })
    }

    pub fn verify_prehash(
        &self,
        hash: &[u8],
        signature: &EcdsaSignatureInner,
    ) -> Result<bool, EcdsaError> {
        let key_size = self.curve.key_size();
        if signature.0.len() != key_size * 2 {
            return Ok(false);
        }

        // SAFETY: FFI call with a valid key handle and valid input slices.
        let status = unsafe {
            BCryptVerifySignature(self.handle.0, None, hash, &signature.0, BCRYPT_FLAGS(0))
        };

        // A signature that simply does not match yields STATUS_INVALID_SIGNATURE,
        // which is a valid "not verified" result rather than an operational error.
        if status == STATUS_INVALID_SIGNATURE {
            return Ok(false);
        }
        status
            .ok()
            .map_err(|e| err(e, "verifying the ECDSA signature"))?;
        Ok(true)
    }

    pub fn public_key_bytes(&self) -> Result<EcdsaPublicKeyComponents, EcdsaError> {
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
        .map_err(|e| err(e, "querying the public key export size"))?;

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
        .map_err(|e| err(e, "exporting the public key"))?;

        // BCrypt ECC public blob layout: BCRYPT_ECCKEY_BLOB header + X + Y
        let header_size = size_of::<BCRYPT_ECCKEY_BLOB>();
        let key_size = self.curve.key_size();

        Ok(EcdsaPublicKeyComponents {
            qx: blob[header_size..header_size + key_size].to_vec(),
            qy: blob[header_size + key_size..header_size + key_size * 2].to_vec(),
        })
    }
}
