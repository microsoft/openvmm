// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using Windows BCrypt APIs.

use super::RsaError;
use super::RsaPublicKeyComponents;
use crate::HashAlgorithm;
use crate::win::CryptAlloc;
use crate::win::KeyHandle;
use std::ffi::CStr;
use std::ffi::c_void;
use windows::Win32::Foundation::NTE_BAD_TYPE;
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
use windows::Win32::Security::Cryptography::BCRYPT_RSAKEY_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_RSAPRIVATE_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_RSAPUBLIC_BLOB;
use windows::Win32::Security::Cryptography::BCRYPT_RSAPUBLIC_MAGIC;
use windows::Win32::Security::Cryptography::CNG_RSA_PRIVATE_KEY_BLOB;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CRYPT_ALGORITHM_IDENTIFIER;
use windows::Win32::Security::Cryptography::CRYPT_DECODE_ALLOC_FLAG;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CRYPT_ENCODE_ALLOC_FLAG;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows::Win32::Security::Cryptography::CRYPT_PRIVATE_KEY_INFO;
use windows::Win32::Security::Cryptography::CryptDecodeObjectEx;
#[cfg(any(test, feature = "test_helpers"))]
use windows::Win32::Security::Cryptography::CryptEncodeObjectEx;
use windows::Win32::Security::Cryptography::PKCS_7_ASN_ENCODING;
use windows::Win32::Security::Cryptography::PKCS_PRIVATE_KEY_INFO;
use windows::Win32::Security::Cryptography::X509_ASN_ENCODING;
use windows::Win32::Security::Cryptography::szOID_RSA_RSA;
#[cfg(any(test, feature = "test_helpers"))]
use windows::core::PSTR;

fn err(err: windows_result::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError(err, op))
}

#[repr(transparent)]
pub(crate) struct RsaKeyPairInner(pub(crate) KeyHandle);

#[repr(transparent)]
pub(crate) struct RsaPublicKeyInner(pub(crate) KeyHandle);

/// Parse a BCRYPT_RSAFULLPRIVATE_BLOB or BCRYPT_RSAPUBLIC_BLOB into its
/// big-endian component byte ranges.
struct PublicComponents<'a> {
    modulus: &'a [u8],
    public_exponent: &'a [u8],
}

/// Export a BCrypt key as the given blob type.
pub(crate) fn export_key(
    key: &KeyHandle,
    blob_type: windows::core::PCWSTR,
) -> Result<Vec<u8>, RsaError> {
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
        // Step 1: parse the PKCS#8 PrivateKeyInfo wrapper to obtain the
        // algorithm OID and the inner PKCS#1 RSAPrivateKey DER.
        let pki_buf = {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mut len: u32 = 0;
            // SAFETY: With CRYPT_DECODE_ALLOC_FLAG, Crypt32 allocates the
            // output buffer and writes its pointer into the location
            // pointed to by pvstructinfo.
            unsafe {
                CryptDecodeObjectEx(
                    X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                    PKCS_PRIVATE_KEY_INFO,
                    der_bytes,
                    CRYPT_DECODE_ALLOC_FLAG,
                    None,
                    Some(std::ptr::from_mut(&mut ptr).cast::<c_void>()),
                    &mut len,
                )
            }
            .map_err(|e| err(e, "decoding PKCS#8 PrivateKeyInfo"))?;
            CryptAlloc::new(ptr, len).map_err(|e| err(e, "decoding PKCS#8 PrivateKeyInfo"))?
        };
        // SAFETY: Crypt32 was asked to decode a PKCS_PRIVATE_KEY_INFO, so
        // the buffer (when non-null and large enough, as validated by
        // as_struct) holds a CRYPT_PRIVATE_KEY_INFO.
        let pki = unsafe { pki_buf.as_struct::<CRYPT_PRIVATE_KEY_INFO>() }
            .map_err(|e| err(e, "decoding PKCS#8 PrivateKeyInfo"))?;
        // SAFETY: pszObjId is a NUL-terminated string owned by the same
        // Crypt32 allocation.
        let oid = unsafe { CStr::from_ptr(pki.Algorithm.pszObjId.0.cast()) };
        // SAFETY: szOID_RSA_RSA is a static NUL-terminated string constant.
        let rsa_oid = unsafe { CStr::from_ptr(szOID_RSA_RSA.0.cast()) };
        if oid != rsa_oid {
            return Err(err(
                windows_result::Error::from_hresult(NTE_BAD_TYPE),
                "PKCS#8 algorithm is not rsaEncryption",
            ));
        }
        if pki.PrivateKey.pbData.is_null() || pki.PrivateKey.cbData == 0 {
            return Err(err(
                windows_result::Error::from_hresult(NTE_BAD_TYPE),
                "PKCS#8 PrivateKey field is empty",
            ));
        }
        // SAFETY: PrivateKey describes a buffer of cbData bytes owned by
        // the same allocation, containing the PKCS#1 RSAPrivateKey DER;
        // validated non-null and non-empty above.
        let pkcs1_der = unsafe {
            std::slice::from_raw_parts(pki.PrivateKey.pbData, pki.PrivateKey.cbData as usize)
        };

        // Step 2: decode the PKCS#1 RSAPrivateKey DER directly into a
        // BCRYPT_RSAKEY_BLOB layout suitable for BCryptImportKeyPair.
        // CNG_RSA_PRIVATE_KEY_BLOB may produce either the BCRYPT_RSAPRIVATE
        // or BCRYPT_RSAFULLPRIVATE layout depending on which fields are
        // present in the source DER; the magic word at the start of the
        // blob distinguishes them.
        let blob_buf = {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mut len: u32 = 0;
            // SAFETY: With CRYPT_DECODE_ALLOC_FLAG, Crypt32 allocates the
            // output buffer and writes its pointer into the location
            // pointed to by pvstructinfo.
            unsafe {
                CryptDecodeObjectEx(
                    X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                    CNG_RSA_PRIVATE_KEY_BLOB,
                    pkcs1_der,
                    CRYPT_DECODE_ALLOC_FLAG,
                    None,
                    Some(std::ptr::from_mut(&mut ptr).cast::<c_void>()),
                    &mut len,
                )
            }
            .map_err(|e| err(e, "decoding PKCS#1 RSA private key"))?;
            CryptAlloc::new(ptr, len).map_err(|e| err(e, "decoding PKCS#1 RSA private key"))?
        };
        let blob = blob_buf.as_bytes();
        if blob.len() < size_of::<BCRYPT_RSAKEY_BLOB>() {
            return Err(err(
                windows_result::Error::from_hresult(NTE_BAD_TYPE),
                "decoded RSA private blob too small",
            ));
        }
        // SAFETY: blob is at least header-sized and the header is POD.
        let magic =
            unsafe { std::ptr::read_unaligned(blob.as_ptr().cast::<BCRYPT_RSAKEY_BLOB>()) }.Magic;
        let blob_type = match magic {
            windows::Win32::Security::Cryptography::BCRYPT_RSAFULLPRIVATE_MAGIC => {
                BCRYPT_RSAFULLPRIVATE_BLOB
            }
            windows::Win32::Security::Cryptography::BCRYPT_RSAPRIVATE_MAGIC => {
                BCRYPT_RSAPRIVATE_BLOB
            }
            _ => {
                return Err(err(
                    windows_result::Error::from_hresult(NTE_BAD_TYPE),
                    "decoded RSA private blob has unexpected magic",
                ));
            }
        };
        let handle = import_key(blob, blob_type)?;
        Ok(Self(handle))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        // Step 1: export the key as a BCrypt full-private blob, which
        // contains every field needed by the PKCS#1 RSAPrivateKey ASN.1
        // SEQUENCE.
        let blob = export_key(&self.0, BCRYPT_RSAFULLPRIVATE_BLOB)?;

        // Step 2: encode the BCrypt blob as PKCS#1 RSAPrivateKey DER.
        // CryptEncodeObjectEx with CNG_RSA_PRIVATE_KEY_BLOB takes the raw
        // BCRYPT_RSAKEY_BLOB layout (header + concatenated components) as
        // its struct input.
        let pkcs1_buf = {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mut len: u32 = 0;
            // SAFETY: With CRYPT_ENCODE_ALLOC_FLAG, Crypt32 allocates the
            // output buffer and writes its pointer into the location
            // pointed to by pvencoded.
            unsafe {
                CryptEncodeObjectEx(
                    X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                    CNG_RSA_PRIVATE_KEY_BLOB,
                    blob.as_ptr().cast::<c_void>(),
                    CRYPT_ENCODE_ALLOC_FLAG,
                    None,
                    Some(std::ptr::from_mut(&mut ptr).cast::<c_void>()),
                    &mut len,
                )
            }
            .map_err(|e| err(e, "encoding PKCS#1 RSA private key"))?;
            CryptAlloc::new(ptr, len).map_err(|e| err(e, "encoding PKCS#1 RSA private key"))?
        };

        // Step 3: wrap the PKCS#1 DER in a PKCS#8 PrivateKeyInfo with the
        // rsaEncryption algorithm OID and ASN.1 NULL parameters.
        let mut null_params = [0x05u8, 0x00];
        let pki = CRYPT_PRIVATE_KEY_INFO {
            Version: 0,
            Algorithm: CRYPT_ALGORITHM_IDENTIFIER {
                // CryptEncodeObjectEx does not mutate pszObjId; the field
                // is `PSTR` only because the same struct is used by the
                // decode direction.
                pszObjId: PSTR(szOID_RSA_RSA.0.cast_mut()),
                Parameters: CRYPT_INTEGER_BLOB {
                    cbData: null_params.len() as u32,
                    pbData: null_params.as_mut_ptr(),
                },
            },
            PrivateKey: {
                let pkcs1 = pkcs1_buf.as_bytes();
                CRYPT_INTEGER_BLOB {
                    cbData: pkcs1.len() as u32,
                    pbData: pkcs1.as_ptr().cast_mut(),
                }
            },
            pAttributes: std::ptr::null_mut(),
        };
        let pkcs8_buf = {
            let mut ptr: *mut c_void = std::ptr::null_mut();
            let mut len: u32 = 0;
            // SAFETY: With CRYPT_ENCODE_ALLOC_FLAG, Crypt32 allocates the
            // output buffer and writes its pointer into the location
            // pointed to by pvencoded.
            unsafe {
                CryptEncodeObjectEx(
                    X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                    PKCS_PRIVATE_KEY_INFO,
                    std::ptr::from_ref(&pki).cast::<c_void>(),
                    CRYPT_ENCODE_ALLOC_FLAG,
                    None,
                    Some(std::ptr::from_mut(&mut ptr).cast::<c_void>()),
                    &mut len,
                )
            }
            .map_err(|e| err(e, "encoding PKCS#8 PrivateKeyInfo"))?;
            CryptAlloc::new(ptr, len).map_err(|e| err(e, "encoding PKCS#8 PrivateKeyInfo"))?
        };
        Ok(pkcs8_buf.as_bytes().to_vec())
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

impl RsaPublicKeyInner {
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        let cb_n = n.len();
        let cb_e = e.len();
        // Bit length of the modulus (a non-negative big-endian integer).
        let bit_length = n
            .iter()
            .enumerate()
            .find_map(|(i, &b)| (b != 0).then(|| ((n.len() - i) as u32) * 8 - b.leading_zeros()))
            .unwrap_or(0);
        let header = BCRYPT_RSAKEY_BLOB {
            Magic: BCRYPT_RSAPUBLIC_MAGIC,
            BitLength: bit_length,
            cbPublicExp: cb_e as u32,
            cbModulus: cb_n as u32,
            cbPrime1: 0,
            cbPrime2: 0,
        };
        // SAFETY: BCRYPT_RSAKEY_BLOB is #[repr(C)] and contains only POD u32
        // fields, so any byte pattern is valid.
        let header_bytes = unsafe {
            std::slice::from_raw_parts(
                std::ptr::from_ref(&header).cast::<u8>(),
                size_of::<BCRYPT_RSAKEY_BLOB>(),
            )
        };
        let mut blob = Vec::with_capacity(size_of::<BCRYPT_RSAKEY_BLOB>() + cb_e + cb_n);
        blob.extend_from_slice(header_bytes);
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
        self.to_components().modulus.len()
    }

    pub fn to_components(&self) -> RsaPublicKeyComponents {
        let blob = export_key(&self.0, BCRYPT_RSAPUBLIC_BLOB).unwrap();
        let components = parse_public_components(&blob);
        RsaPublicKeyComponents {
            modulus: components.modulus.to_vec(),
            public_exponent: components.public_exponent.to_vec(),
        }
    }
}
