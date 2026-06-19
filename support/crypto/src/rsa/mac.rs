// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! RSA implementation using macOS Security.framework SecKey APIs.

// UNSAFETY: calling Security.framework and CoreFoundation C APIs via FFI.
#![expect(unsafe_code)]

use super::RsaError;
use crate::HashAlgorithm;
use crate::mac::*;
use der::Decode;
use der::Encode;
use std::ptr;

/// errSecVerifyFailed — signature verification failed (i.e. signature is
/// invalid). Other error codes are infrastructure failures.
const ERR_SEC_VERIFY_FAILED: CFIndex = -67808;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecAttrKeyType: CFStringRef;
    static kSecAttrKeyTypeRSA: CFStringRef;
    static kSecAttrKeySizeInBits: CFStringRef;
    static kSecAttrKeyClass: CFStringRef;
    static kSecAttrKeyClassPrivate: CFStringRef;
    static kSecAttrKeyClassPublic: CFStringRef;

    fn SecKeyCreateRandomKey(parameters: CFDictionaryRef, error: *mut CFErrorRef) -> SecKeyRef;
    fn SecKeyCreateWithData(
        key_data: CFDataRef,
        attributes: CFDictionaryRef,
        error: *mut CFErrorRef,
    ) -> SecKeyRef;
    fn SecKeyCopyPublicKey(key: SecKeyRef) -> SecKeyRef;
    fn SecKeyCopyExternalRepresentation(key: SecKeyRef, error: *mut CFErrorRef) -> CFDataRef;
    fn SecKeyGetBlockSize(key: SecKeyRef) -> usize;
    fn SecKeyCreateSignature(
        key: SecKeyRef,
        algorithm: CFStringRef,
        data_to_sign: CFDataRef,
        error: *mut CFErrorRef,
    ) -> CFDataRef;
    fn SecKeyVerifySignature(
        key: SecKeyRef,
        algorithm: CFStringRef,
        signed_data: CFDataRef,
        signature: CFDataRef,
        error: *mut CFErrorRef,
    ) -> u8;
    fn SecKeyCreateEncryptedData(
        key: SecKeyRef,
        algorithm: CFStringRef,
        plaintext: CFDataRef,
        error: *mut CFErrorRef,
    ) -> CFDataRef;
    fn SecKeyCreateDecryptedData(
        key: SecKeyRef,
        algorithm: CFStringRef,
        ciphertext: CFDataRef,
        error: *mut CFErrorRef,
    ) -> CFDataRef;
}

fn der_err(e: der::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Der(e, op))
}

fn pkcs8_err(e: pkcs8::Error, op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Pkcs8(e, op))
}

fn null_err(op: &'static str) -> RsaError {
    RsaError(crate::BackendError::Null(op))
}

/// Wrap a shared `crate::mac::cf_data` call with the RSA error type.
fn rsa_cf_data(bytes: &[u8], op: &'static str) -> Result<CfHandle, RsaError> {
    cf_data(bytes, op).map_err(RsaError)
}

/// Wrap `crate::mac::cf_number` with the RSA error type.
fn rsa_cf_number(value: i32, op: &'static str) -> Result<CfHandle, RsaError> {
    cf_number(value, op).map_err(RsaError)
}

/// Wrap `crate::mac::cf_dict` with the RSA error type.
fn rsa_cf_dict(pairs: &[(CFTypeRef, CFTypeRef)], op: &'static str) -> Result<CfHandle, RsaError> {
    cf_dict(pairs, op).map_err(RsaError)
}

/// Wrap `crate::mac::sec_err` with the RSA error type.
///
/// # Safety
///
/// Same as `crate::mac::sec_err`.
unsafe fn rsa_sec_err(error: CFErrorRef, op: &'static str) -> RsaError {
    // SAFETY: per caller contract.
    RsaError(unsafe { sec_err(error, op) })
}

/// Build a PKCS#1 RSAPublicKey DER blob from (n, e).
fn build_pkcs1_pub(n: &[u8], e: &[u8]) -> Result<Vec<u8>, RsaError> {
    use der::asn1::UintRef;
    let pk = pkcs1::RsaPublicKey {
        modulus: UintRef::new(n).map_err(|e| der_err(e, "encoding modulus"))?,
        public_exponent: UintRef::new(e).map_err(|e| der_err(e, "encoding e"))?,
    };
    pk.to_der()
        .map_err(|e| der_err(e, "encoding PKCS#1 RSA public key"))
}

/// Parse a PKCS#1 RSAPublicKey DER blob into (n, e) byte vectors.
fn parse_pkcs1_pub(der_bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), RsaError> {
    let pk = pkcs1::RsaPublicKey::from_der(der_bytes)
        .map_err(|e| der_err(e, "parsing PKCS#1 RSA public key"))?;
    Ok((
        pk.modulus.as_bytes().to_vec(),
        pk.public_exponent.as_bytes().to_vec(),
    ))
}

/// Import an RSA private key from a PKCS#1 RSAPrivateKey DER blob.
fn import_private(pkcs1_der: &[u8]) -> Result<CfHandle, RsaError> {
    let key_data = rsa_cf_data(pkcs1_der, "create CFData for PKCS#1 RSA private key")?;
    // SAFETY: kSecAttr* are valid CFStringRef extern statics.
    let attrs = unsafe {
        rsa_cf_dict(
            &[
                (kSecAttrKeyType, kSecAttrKeyTypeRSA),
                (kSecAttrKeyClass, kSecAttrKeyClassPrivate),
            ],
            "create attrs for RSA private key import",
        )?
    };
    let mut error: CFErrorRef = ptr::null();
    // SAFETY: key_data and attrs are valid CF objects.
    let key = unsafe { SecKeyCreateWithData(key_data.0, attrs.0, &mut error) };
    if key.is_null() {
        // SAFETY: error is either null or a valid CFErrorRef.
        return Err(unsafe { rsa_sec_err(error, "import RSA private key") });
    }
    Ok(CfHandle(key))
}

/// Import an RSA public key from a PKCS#1 RSAPublicKey DER blob.
fn import_public(pkcs1_der: &[u8]) -> Result<CfHandle, RsaError> {
    let key_data = rsa_cf_data(pkcs1_der, "create CFData for PKCS#1 RSA public key")?;
    // SAFETY: kSecAttr* are valid CFStringRef extern statics.
    let attrs = unsafe {
        rsa_cf_dict(
            &[
                (kSecAttrKeyType, kSecAttrKeyTypeRSA),
                (kSecAttrKeyClass, kSecAttrKeyClassPublic),
            ],
            "create attrs for RSA public key import",
        )?
    };
    let mut error: CFErrorRef = ptr::null();
    // SAFETY: key_data and attrs are valid.
    let key = unsafe { SecKeyCreateWithData(key_data.0, attrs.0, &mut error) };
    if key.is_null() {
        // SAFETY: error is null or valid.
        return Err(unsafe { rsa_sec_err(error, "import RSA public key") });
    }
    Ok(CfHandle(key))
}

#[repr(transparent)]
pub(crate) struct RsaKeyPairInner(CfHandle);

#[repr(transparent)]
pub(crate) struct RsaPublicKeyInner(CfHandle);

impl RsaKeyPairInner {
    pub fn generate(bits: u32) -> Result<Self, RsaError> {
        let size = rsa_cf_number(bits as i32, "create CFNumber for key size")?;
        // SAFETY: kSecAttr* are valid CFStringRef extern statics.
        let params = unsafe {
            rsa_cf_dict(
                &[
                    (kSecAttrKeyType, kSecAttrKeyTypeRSA),
                    (kSecAttrKeySizeInBits, size.0),
                ],
                "create params for RSA key generation",
            )?
        };
        let mut error: CFErrorRef = ptr::null();
        // SAFETY: params is a valid CFDictionary.
        let key = unsafe { SecKeyCreateRandomKey(params.0, &mut error) };
        if key.is_null() {
            // SAFETY: error is null or valid.
            return Err(unsafe { rsa_sec_err(error, "generate RSA key") });
        }
        Ok(Self(CfHandle(key)))
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
        let handle = import_private(pki.private_key.as_bytes())?;
        Ok(Self(handle))
    }

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>, RsaError> {
        use der::asn1::OctetString;
        use pkcs8::spki::AlgorithmIdentifierOwned;

        let mut error: CFErrorRef = ptr::null();
        // SAFETY: self.0.0 is a valid SecKeyRef.
        let data = unsafe { SecKeyCopyExternalRepresentation(self.0.0, &mut error) };
        if data.is_null() {
            // SAFETY: error is null or valid.
            return Err(unsafe { rsa_sec_err(error, "export RSA private key") });
        }
        let data = CfHandle(data);
        // SAFETY: data.0 is a valid CFDataRef.
        let pkcs1_der = unsafe { cf_data_to_vec(data.0) };

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
        let ct = rsa_cf_data(input, "create CFData")?;
        let mut error: CFErrorRef = ptr::null();
        // SAFETY: self.0.0 and ct.0 are valid.
        let pt = unsafe {
            SecKeyCreateDecryptedData(
                self.0.0,
                hash_algorithm.sec_key_alg_rsa_oaep(),
                ct.0,
                &mut error,
            )
        };
        if pt.is_null() {
            // SAFETY: error is null or valid.
            return Err(unsafe { rsa_sec_err(error, "RSA-OAEP decrypt") });
        }
        let pt = CfHandle(pt);
        // SAFETY: pt.0 is valid.
        Ok(unsafe { cf_data_to_vec(pt.0) })
    }

    pub fn pkcs1_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        sign(&self.0, hash_algorithm.sec_key_alg_rsa_pkcs1(), data)
    }

    pub fn pss_sign(
        &self,
        data: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        sign(&self.0, hash_algorithm.sec_key_alg_rsa_pss(), data)
    }

    pub(crate) fn as_pub(&self) -> &RsaPublicKeyInner {
        // SAFETY: both are #[repr(transparent)] over CfHandle. A SecKeyRef
        // for a private key supports the SecKeyCopyPublicKey-style read
        // path used by RsaPublicKeyInner methods.
        unsafe { std::mem::transmute::<&RsaKeyPairInner, &RsaPublicKeyInner>(self) }
    }
}

fn sign(key: &CfHandle, alg: CFStringRef, data: &[u8]) -> Result<Vec<u8>, RsaError> {
    let data_cf = rsa_cf_data(data, "create CFData")?;
    let mut error: CFErrorRef = ptr::null();
    // SAFETY: key, alg, data_cf are valid.
    let sig = unsafe { SecKeyCreateSignature(key.0, alg, data_cf.0, &mut error) };
    if sig.is_null() {
        // SAFETY: error is null or valid.
        return Err(unsafe { rsa_sec_err(error, "RSA sign") });
    }
    let sig = CfHandle(sig);
    // SAFETY: sig.0 valid.
    Ok(unsafe { cf_data_to_vec(sig.0) })
}

fn verify(
    key: &CfHandle,
    alg: CFStringRef,
    message: &[u8],
    signature: &[u8],
) -> Result<bool, RsaError> {
    let msg = rsa_cf_data(message, "create CFData for message")?;
    let sig = rsa_cf_data(signature, "create CFData for signature")?;
    let mut error: CFErrorRef = ptr::null();
    // SAFETY: all CF refs are valid.
    let ok = unsafe { SecKeyVerifySignature(key.0, alg, msg.0, sig.0, &mut error) };
    if ok != 0 {
        return Ok(true);
    }
    // SAFETY: error is null or a valid CFErrorRef.
    match unsafe { cf_error_code(error) } {
        // Null error or errSecVerifyFailed both mean "bad signature" rather
        // than an infrastructure failure.
        None | Some(ERR_SEC_VERIFY_FAILED) => {
            if !error.is_null() {
                // SAFETY: error is a valid CFErrorRef we own.
                unsafe { CFRelease(error) };
            }
            Ok(false)
        }
        // SAFETY: error is a valid CFErrorRef (sec_err takes ownership).
        Some(_) => Err(unsafe { rsa_sec_err(error, "RSA verify") }),
    }
}

impl RsaPublicKeyInner {
    pub fn from_components(n: &[u8], e: &[u8]) -> Result<Self, RsaError> {
        let der = build_pkcs1_pub(n, e)?;
        Ok(Self(import_public(&der)?))
    }

    pub fn oaep_encrypt(
        &self,
        input: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<Vec<u8>, RsaError> {
        let pub_key = self.public_key_handle()?;
        let pt = rsa_cf_data(input, "create CFData")?;
        let mut error: CFErrorRef = ptr::null();
        // SAFETY: pub_key and pt valid.
        let ct = unsafe {
            SecKeyCreateEncryptedData(
                pub_key.0,
                hash_algorithm.sec_key_alg_rsa_oaep(),
                pt.0,
                &mut error,
            )
        };
        if ct.is_null() {
            // SAFETY: error is null or valid.
            return Err(unsafe { rsa_sec_err(error, "RSA-OAEP encrypt") });
        }
        let ct = CfHandle(ct);
        // SAFETY: ct.0 valid.
        Ok(unsafe { cf_data_to_vec(ct.0) })
    }

    pub fn pkcs1_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let pub_key = self.public_key_handle()?;
        verify(
            &pub_key,
            hash_algorithm.sec_key_alg_rsa_pkcs1(),
            message,
            signature,
        )
    }

    pub fn pss_verify(
        &self,
        message: &[u8],
        signature: &[u8],
        hash_algorithm: HashAlgorithm,
    ) -> Result<bool, RsaError> {
        let pub_key = self.public_key_handle()?;
        verify(
            &pub_key,
            hash_algorithm.sec_key_alg_rsa_pss(),
            message,
            signature,
        )
    }

    pub fn modulus_size(&self) -> usize {
        // SAFETY: self.0.0 is a valid SecKeyRef.
        unsafe { SecKeyGetBlockSize(self.0.0) }
    }

    pub fn to_components(&self) -> super::RsaPublicKeyComponents {
        let pub_key = self.public_key_handle().unwrap();
        let mut error: CFErrorRef = ptr::null();
        // SAFETY: pub_key.0 is valid.
        let data = unsafe { SecKeyCopyExternalRepresentation(pub_key.0, &mut error) };
        assert!(!data.is_null(), "SecKeyCopyExternalRepresentation failed");
        let data = CfHandle(data);
        // SAFETY: data.0 is valid.
        let der_bytes = unsafe { cf_data_to_vec(data.0) };
        let (modulus, public_exponent) = parse_pkcs1_pub(&der_bytes).unwrap();
        super::RsaPublicKeyComponents {
            modulus,
            public_exponent,
        }
    }

    /// Get a SecKeyRef for the public half of this key. For a key already
    /// representing only the public half, this is just a retained copy.
    fn public_key_handle(&self) -> Result<CfHandle, RsaError> {
        // SAFETY: self.0.0 is a valid SecKeyRef.
        let pk = unsafe { SecKeyCopyPublicKey(self.0.0) };
        if pk.is_null() {
            return Err(null_err("SecKeyCopyPublicKey"));
        }
        Ok(CfHandle(pk))
    }
}
