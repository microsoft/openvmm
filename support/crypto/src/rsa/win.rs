// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using Windows BCrypt APIs.

use super::RsaError;
use crate::HashAlgorithm;
use crate::win::KeyHandle;
use der::Decode;
#[cfg(any(test, feature = "test_helpers"))]
use der::Encode;
use std::ffi::c_void;
use windows::Win32::Foundation::STATUS_INVALID_PARAMETER;
use windows::Win32::Foundation::STATUS_INVALID_SIGNATURE;
use windows::Win32::Security::Cryptography::BCRYPT_FLAGS;
use windows::Win32::Security::Cryptography::BCRYPT_KEY_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_OAEP_PADDING_INFO;
use windows::Win32::Security::Cryptography::BCRYPT_PAD_OAEP;
use windows::Win32::Security::Cryptography::BCRYPT_PAD_PKCS1;
use windows::Win32::Security::Cryptography::BCRYPT_PAD_PSS;
use windows::Win32::Security::Cryptography::BCRYPT_PKCS1_PADDING_INFO;
use windows::Win32::Security::Cryptography::BCRYPT_PSS_PADDING_INFO;
use windows::Win32::Security::Cryptography::BCRYPT_RSA_ALG_HANDLE;
use windows::Win32::Security::Cryptography::BCRYPT_RSAFULLPRIVATE_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_RSAFULLPRIVATE_MAGIC;
use windows::Win32::Security::Cryptography::BCRYPT_RSAKEY_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_RSAPUBLIC_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_RSAPUBLIC_MAGIC;

fn err(err: windows_result::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Bcrypt(err, op))
}

fn der_err(e: der::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Der(e, op))
}

fn pkcs8_err(e: pkcs8::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Pkcs8(e, op))
}

#[repr(transparent)]
pub struct RsaKeyPairInner(KeyHandle);

#[repr(transparent)]
pub struct RsaPublicKeyInner(KeyHandle);

/// Left-pads `src` with leading zero bytes so the result is `len` bytes long.
/// Panics if `src.len() > len`.
fn left_pad(src: &[u8], len: usize) -> Vec<u8> {
    assert!(src.len() <= len);
    let mut out = vec![0u8; len];
    out[len - src.len()..].copy_from_slice(src);
    out
}

/// Bit length of a non-negative big-endian integer.
fn bit_length(bytes: &[u8]) -> u32 {
    for (i, &b) in bytes.iter().enumerate() {
        if b != 0 {
            return ((bytes.len() - i) as u32) * 8 - b.leading_zeros();
        }
    }
    0
}

/// Build a BCRYPT_RSAFULLPRIVATE_BLOB from PKCS#1 RSA private key components.
fn build_fullprivate_blob(key: &pkcs1::RsaPrivateKey<'_>) -> Vec<u8> {
    let n = key.modulus.as_bytes();
    let e = key.public_exponent.as_bytes();
    let d = key.private_exponent.as_bytes();
    let p = key.prime1.as_bytes();
    let q = key.prime2.as_bytes();
    let dp = key.exponent1.as_bytes();
    let dq = key.exponent2.as_bytes();
    let qinv = key.coefficient.as_bytes();

    let cb_modulus = n.len().max(d.len());
    let cb_prime1 = p.len().max(dp.len()).max(qinv.len());
    let cb_prime2 = q.len().max(dq.len());
    let cb_public_exp = e.len();

    let header = BCRYPT_RSAKEY_BLOB {
        Magic: BCRYPT_RSAFULLPRIVATE_MAGIC,
        BitLength: bit_length(n),
        cbPublicExp: cb_public_exp as u32,
        cbModulus: cb_modulus as u32,
        cbPrime1: cb_prime1 as u32,
        cbPrime2: cb_prime2 as u32,
    };

    let total = size_of::<BCRYPT_RSAKEY_BLOB>()
        + cb_public_exp
        + cb_modulus
        + cb_prime1 * 3
        + cb_prime2 * 2
        + cb_modulus;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(&left_pad(e, cb_public_exp));
    out.extend_from_slice(&left_pad(n, cb_modulus));
    out.extend_from_slice(&left_pad(p, cb_prime1));
    out.extend_from_slice(&left_pad(q, cb_prime2));
    out.extend_from_slice(&left_pad(dp, cb_prime1));
    out.extend_from_slice(&left_pad(dq, cb_prime2));
    out.extend_from_slice(&left_pad(qinv, cb_prime1));
    out.extend_from_slice(&left_pad(d, cb_modulus));
    out
}

/// SAFETY: zerocopy IntoBytes-compatible because BCRYPT_RSAKEY_BLOB is
/// `#[repr(C)]` with all-integer fields. We can't derive `IntoBytes` on a
/// foreign type, so write a small helper.
trait HeaderBytes {
    fn as_bytes(&self) -> &[u8];
}
impl HeaderBytes for BCRYPT_RSAKEY_BLOB {
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: BCRYPT_RSAKEY_BLOB is #[repr(C)] and contains only POD u32
        // fields, so any byte pattern is valid.
        unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(self).cast::<u8>(),
                size_of::<BCRYPT_RSAKEY_BLOB>(),
            )
        }
    }
}

/// Parse a BCRYPT_RSAFULLPRIVATE_BLOB or BCRYPT_RSAPUBLIC_BLOB into its
/// big-endian component byte ranges.
struct PublicComponents<'a> {
    modulus: &'a [u8],
    public_exponent: &'a [u8],
}

fn parse_public_components(blob: &[u8]) -> PublicComponents<'_> {
    assert!(blob.len() >= size_of::<BCRYPT_RSAKEY_BLOB>());
    let header_ptr = blob.as_ptr().cast::<BCRYPT_RSAKEY_BLOB>();
    // SAFETY: blob is at least header-sized; header is POD.
    let header = unsafe { std::ptr::read_unaligned(header_ptr) };
    let off = size_of::<BCRYPT_RSAKEY_BLOB>();
    let cb_e = header.cbPublicExp as usize;
    let cb_n = header.cbModulus as usize;
    PublicComponents {
        public_exponent: &blob[off..off + cb_e],
        modulus: &blob[off + cb_e..off + cb_e + cb_n],
    }
}

/// Export a BCrypt key as the given blob type.
fn export_key(key: &KeyHandle, blob_type: windows::core::PCWSTR) -> Result<Vec<u8>, RsaError> {
    let mut needed: u32 = 0;
    // SAFETY: handle is valid; first call queries needed size.
    unsafe {
        windows::Win32::Security::Cryptography::BCryptExportKey(
            key.0,
            None,
            blob_type,
            None,
            &mut needed,
            0,
        )
    }
    .ok()
    .map_err(|e| err(e, "querying export size"))?;
    let mut out = vec![0u8; needed as usize];
    // SAFETY: out is sized per the previous query.
    unsafe {
        windows::Win32::Security::Cryptography::BCryptExportKey(
            key.0,
            None,
            blob_type,
            Some(&mut out),
            &mut needed,
            0,
        )
    }
    .ok()
    .map_err(|e| err(e, "exporting key"))?;
    out.truncate(needed as usize);
    Ok(out)
}

/// Import a key blob into a BCrypt key handle.
fn import_key(blob: &[u8], blob_type: windows::core::PCWSTR) -> Result<KeyHandle, RsaError> {
    let mut handle = BCRYPT_KEY_HANDLE::default();
    // SAFETY: BCRYPT_RSA_ALG_HANDLE is a static pseudo-handle; blob is a
    // valid byte slice.
    unsafe {
        windows::Win32::Security::Cryptography::BCryptImportKeyPair(
            BCRYPT_RSA_ALG_HANDLE,
            None,
            blob_type,
            &mut handle,
            blob,
            0,
        )
    }
    .ok()
    .map_err(|e| err(e, "importing key"))?;
    Ok(KeyHandle(handle))
}

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let mut handle = BCRYPT_KEY_HANDLE::default();
        // SAFETY: BCRYPT_RSA_ALG_HANDLE is a static pseudo-handle valid for
        // the lifetime of the process.
        unsafe {
            windows::Win32::Security::Cryptography::BCryptGenerateKeyPair(
                BCRYPT_RSA_ALG_HANDLE,
                &mut handle,
                bits,
                0,
            )
        }
        .ok()
        .map_err(|e| err(e, "generating RSA key"))?;
        let key = KeyHandle(handle);
        // SAFETY: handle is valid.
        unsafe { windows::Win32::Security::Cryptography::BCryptFinalizeKeyPair(key.0, 0) }
            .ok()
            .map_err(|e| err(e, "finalizing RSA key"))?;
        Ok(Self(key))
    }

    pub fn from_pkcs8_der(der_bytes: &[u8]) -> Result<Self, RsaError> {
        let pki = pkcs8::PrivateKeyInfoRef::from_der(der_bytes)
            .map_err(|e| der_err(e, "parsing PKCS#8 DER"))?;
        if pki.algorithm.oid != pkcs1::ALGORITHM_OID {
            return Err(pkcs8_err(
                pkcs8::Error::KeyMalformed(pkcs8::KeyError::Invalid),
                "PKCS#8 algorithm is not rsaEncryption",
            ));
        }
        let key = pkcs1::RsaPrivateKey::from_der(pki.private_key.as_bytes())
            .map_err(|e| der_err(e, "parsing PKCS#1 RSA private key"))?;
        if key.other_prime_infos.is_some() {
            return Err(pkcs8_err(
                pkcs8::Error::KeyMalformed(pkcs8::KeyError::Invalid),
                "multiprime RSA keys not supported",
            ));
        }
        let blob = build_fullprivate_blob(&key);
        let handle = import_key(&blob, BCRYPT_RSAFULLPRIVATE_BLOB)?;
        Ok(Self(handle))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        use der::asn1::OctetString;
        use der::asn1::UintRef;
        use pkcs8::spki::AlgorithmIdentifierOwned;

        let blob = export_key(&self.0, BCRYPT_RSAFULLPRIVATE_BLOB)?;
        let header_ptr = blob.as_ptr().cast::<BCRYPT_RSAKEY_BLOB>();
        // SAFETY: blob is at least header-sized and header is POD.
        let header = unsafe { std::ptr::read_unaligned(header_ptr) };
        let mut off = size_of::<BCRYPT_RSAKEY_BLOB>();
        let cb_e = header.cbPublicExp as usize;
        let cb_n = header.cbModulus as usize;
        let cb_p = header.cbPrime1 as usize;
        let cb_q = header.cbPrime2 as usize;
        let take = |off: &mut usize, n: usize| {
            let s = &blob[*off..*off + n];
            *off += n;
            s
        };
        let e_bytes = take(&mut off, cb_e);
        let n_bytes = take(&mut off, cb_n);
        let p_bytes = take(&mut off, cb_p);
        let q_bytes = take(&mut off, cb_q);
        let dp_bytes = take(&mut off, cb_p);
        let dq_bytes = take(&mut off, cb_q);
        let qinv_bytes = take(&mut off, cb_p);
        let d_bytes = take(&mut off, cb_n);

        let pk = pkcs1::RsaPrivateKey {
            modulus: UintRef::new(n_bytes).map_err(|e| der_err(e, "encoding modulus"))?,
            public_exponent: UintRef::new(e_bytes).map_err(|e| der_err(e, "encoding e"))?,
            private_exponent: UintRef::new(d_bytes).map_err(|e| der_err(e, "encoding d"))?,
            prime1: UintRef::new(p_bytes).map_err(|e| der_err(e, "encoding p"))?,
            prime2: UintRef::new(q_bytes).map_err(|e| der_err(e, "encoding q"))?,
            exponent1: UintRef::new(dp_bytes).map_err(|e| der_err(e, "encoding dp"))?,
            exponent2: UintRef::new(dq_bytes).map_err(|e| der_err(e, "encoding dq"))?,
            coefficient: UintRef::new(qinv_bytes).map_err(|e| der_err(e, "encoding qinv"))?,
            other_prime_infos: None,
        };
        let pkcs1_der = pk
            .to_der()
            .map_err(|e| der_err(e, "encoding PKCS#1 RSA private key"))?;

        let pki = pkcs8::PrivateKeyInfoOwned {
            algorithm: AlgorithmIdentifierOwned {
                oid: pkcs1::ALGORITHM_OID,
                parameters: Some(der::Any::null()),
            },
            private_key: OctetString::new(pkcs1_der)
                .map_err(|e| der_err(e, "wrapping PKCS#1 in OCTET STRING"))?,
            public_key: None,
        };
        pki.to_der()
            .map_err(|e| der_err(e, "encoding PKCS#8 PrivateKeyInfo"))
    }

    pub fn oaep_decrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let alg_id = hash_algorithm.bcrypt_alg_id();
        let pad = BCRYPT_OAEP_PADDING_INFO {
            pszAlgId: alg_id,
            pbLabel: std::ptr::null_mut(),
            cbLabel: 0,
        };
        let pad_ptr: *const c_void = std::ptr::from_ref(&pad).cast();
        let mut needed: u32 = 0;
        // SAFETY: handle is valid; first call queries needed size.
        unsafe {
            windows::Win32::Security::Cryptography::BCryptDecrypt(
                self.0.0,
                Some(input),
                Some(pad_ptr),
                None,
                None,
                &mut needed,
                BCRYPT_PAD_OAEP,
            )
        }
        .ok()
        .map_err(|e| err(e, "querying OAEP decrypt size"))?;
        let mut out = vec![0u8; needed as usize];
        // SAFETY: output buffer is sized to needed.
        unsafe {
            windows::Win32::Security::Cryptography::BCryptDecrypt(
                self.0.0,
                Some(input),
                Some(pad_ptr),
                None,
                Some(&mut out),
                &mut needed,
                BCRYPT_PAD_OAEP,
            )
        }
        .ok()
        .map_err(|e| err(e, "RSA-OAEP decrypt"))?;
        out.truncate(needed as usize);
        Ok(out)
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let digest = hash_algorithm.hash(data);
        let pad = BCRYPT_PKCS1_PADDING_INFO {
            pszAlgId: hash_algorithm.bcrypt_alg_id(),
        };
        sign_hash(
            &self.0,
            &digest,
            std::ptr::from_ref(&pad).cast(),
            BCRYPT_PAD_PKCS1,
        )
    }

    pub fn pss_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let digest = hash_algorithm.hash(data);
        let pad = BCRYPT_PSS_PADDING_INFO {
            pszAlgId: hash_algorithm.bcrypt_alg_id(),
            cbSalt: hash_algorithm.output_size() as u32,
        };
        sign_hash(
            &self.0,
            &digest,
            std::ptr::from_ref(&pad).cast(),
            BCRYPT_PAD_PSS,
        )
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: both are #[repr(transparent)] over KeyHandle; a private
        // key handle is valid for any public-key operation.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

fn sign_hash(
    key: &KeyHandle,
    digest: &[u8],
    pad_info: *const c_void,
    flags: BCRYPT_FLAGS,
) -> Result<Vec<u8>, RsaError> {
    let mut needed: u32 = 0;
    // SAFETY: handle and digest valid; first call queries needed size.
    unsafe {
        windows::Win32::Security::Cryptography::BCryptSignHash(
            key.0,
            Some(pad_info),
            digest,
            None,
            &mut needed,
            flags,
        )
    }
    .ok()
    .map_err(|e| err(e, "querying signature size"))?;
    let mut sig = vec![0u8; needed as usize];
    // SAFETY: sig is sized per query.
    unsafe {
        windows::Win32::Security::Cryptography::BCryptSignHash(
            key.0,
            Some(pad_info),
            digest,
            Some(&mut sig),
            &mut needed,
            flags,
        )
    }
    .ok()
    .map_err(|e| err(e, "signing hash"))?;
    sig.truncate(needed as usize);
    Ok(sig)
}

fn verify_hash(
    key: &KeyHandle,
    digest: &[u8],
    signature: &[u8],
    pad_info: *const c_void,
    flags: BCRYPT_FLAGS,
) -> Result<bool, RsaError> {
    // SAFETY: all pointers/handles are valid for the call.
    let status = unsafe {
        windows::Win32::Security::Cryptography::BCryptVerifySignature(
            key.0,
            Some(pad_info),
            digest,
            signature,
            flags,
        )
    };
    if status == STATUS_INVALID_SIGNATURE || status == STATUS_INVALID_PARAMETER {
        // STATUS_INVALID_PARAMETER is what BCrypt returns when the
        // signature bytes don't decode to a valid encoded message (e.g.
        // wrong length, or as an integer >= modulus). Treat that as
        // "bad signature" rather than an infrastructure error, matching
        // the symcrypt backend.
        return Ok(false);
    }
    status.ok().map_err(|e| err(e, "verifying signature"))?;
    Ok(true)
}

impl RsaPublicKeyInner {
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        let cb_n = n.len();
        let cb_e = e.len();
        let header = BCRYPT_RSAKEY_BLOB {
            Magic: BCRYPT_RSAPUBLIC_MAGIC,
            BitLength: bit_length(n),
            cbPublicExp: cb_e as u32,
            cbModulus: cb_n as u32,
            cbPrime1: 0,
            cbPrime2: 0,
        };
        let mut blob = Vec::with_capacity(size_of::<BCRYPT_RSAKEY_BLOB>() + cb_e + cb_n);
        blob.extend_from_slice(header.as_bytes());
        blob.extend_from_slice(e);
        blob.extend_from_slice(n);
        let handle = import_key(&blob, BCRYPT_RSAPUBLIC_BLOB)?;
        Ok(Self(handle))
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let alg_id = hash_algorithm.bcrypt_alg_id();
        let pad = BCRYPT_OAEP_PADDING_INFO {
            pszAlgId: alg_id,
            pbLabel: std::ptr::null_mut(),
            cbLabel: 0,
        };
        let pad_ptr: *const c_void = std::ptr::from_ref(&pad).cast();
        let mut needed: u32 = 0;
        // SAFETY: handle valid; first call queries needed size.
        unsafe {
            windows::Win32::Security::Cryptography::BCryptEncrypt(
                self.0.0,
                Some(input),
                Some(pad_ptr),
                None,
                None,
                &mut needed,
                BCRYPT_PAD_OAEP,
            )
        }
        .ok()
        .map_err(|e| err(e, "querying OAEP encrypt size"))?;
        let mut out = vec![0u8; needed as usize];
        // SAFETY: output sized to needed.
        unsafe {
            windows::Win32::Security::Cryptography::BCryptEncrypt(
                self.0.0,
                Some(input),
                Some(pad_ptr),
                None,
                Some(&mut out),
                &mut needed,
                BCRYPT_PAD_OAEP,
            )
        }
        .ok()
        .map_err(|e| err(e, "RSA-OAEP encrypt"))?;
        out.truncate(needed as usize);
        Ok(out)
    }

    pub fn pkcs1_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let digest = hash_algorithm.hash(message);
        let pad = BCRYPT_PKCS1_PADDING_INFO {
            pszAlgId: hash_algorithm.bcrypt_alg_id(),
        };
        verify_hash(
            &self.0,
            &digest,
            signature,
            std::ptr::from_ref(&pad).cast(),
            BCRYPT_PAD_PKCS1,
        )
    }

    pub fn pss_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let digest = hash_algorithm.hash(message);
        let pad = BCRYPT_PSS_PADDING_INFO {
            pszAlgId: hash_algorithm.bcrypt_alg_id(),
            cbSalt: hash_algorithm.output_size() as u32,
        };
        verify_hash(
            &self.0,
            &digest,
            signature,
            std::ptr::from_ref(&pad).cast(),
            BCRYPT_PAD_PSS,
        )
    }

    pub fn modulus_size(&self) -> usize {
        self.modulus().len()
    }

    pub fn modulus(&self) -> Vec<u8> {
        let blob = export_key(&self.0, BCRYPT_RSAPUBLIC_BLOB).unwrap();
        parse_public_components(&blob).modulus.to_vec()
    }

    pub fn public_exponent(&self) -> Vec<u8> {
        let blob = export_key(&self.0, BCRYPT_RSAPUBLIC_BLOB).unwrap();
        parse_public_components(&blob).public_exponent.to_vec()
    }
}
