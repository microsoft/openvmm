// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hash algorithms and related utilities.

#![expect(deprecated)]

/// Hash algorithm for RSA operations.
#[derive(Debug, Clone, Copy)]
pub enum HashAlgorithm {
    /// SHA-1
    #[deprecated(note = "SHA-1 is considered weak and should not be used for new applications")]
    Sha1,
    /// SHA-256
    Sha256,
    /// SHA-384
    Sha384,
    /// SHA-512
    Sha512,
}

impl HashAlgorithm {
    #[cfg(openssl)]
    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        openssl::hash::hash(self.into(), data).unwrap().to_vec()
    }

    #[cfg(symcrypt)]
    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            HashAlgorithm::Sha1 => symcrypt::hash::sha1(data).to_vec(),
            HashAlgorithm::Sha256 => symcrypt::hash::sha256(data).to_vec(),
            HashAlgorithm::Sha384 => symcrypt::hash::sha384(data).to_vec(),
            HashAlgorithm::Sha512 => symcrypt::hash::sha512(data).to_vec(),
        }
    }

    #[cfg(rust)]
    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            HashAlgorithm::Sha1 => sha1::Sha1::digest(data).to_vec(),
            HashAlgorithm::Sha256 => sha2::Sha256::digest(data).to_vec(),
            HashAlgorithm::Sha384 => sha2::Sha384::digest(data).to_vec(),
            HashAlgorithm::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }

    #[cfg(all(native, windows))]
    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; self.output_size()];
        // SAFETY: the handle is a static pseudo-handle valid for the process;
        // the input and output slices are valid for the call. Status must be
        // success because all inputs are known-good (fixed alg, fixed-size
        // output buffer matching the digest size).
        unsafe {
            windows::Win32::Security::Cryptography::BCryptHash(
                self.bcrypt_handle(),
                None,
                data,
                &mut out,
            )
            .unwrap();
        }
        out
    }

    #[cfg(all(native, windows))]
    pub(crate) fn bcrypt_handle(self) -> windows::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE {
        use windows::Win32::Security::Cryptography::*;
        match self {
            HashAlgorithm::Sha1 => BCRYPT_SHA1_ALG_HANDLE,
            HashAlgorithm::Sha256 => BCRYPT_SHA256_ALG_HANDLE,
            HashAlgorithm::Sha384 => BCRYPT_SHA384_ALG_HANDLE,
            HashAlgorithm::Sha512 => BCRYPT_SHA512_ALG_HANDLE,
        }
    }

    #[cfg(all(native, windows))]
    pub(crate) fn bcrypt_alg_id(self) -> windows::core::PCWSTR {
        use windows::Win32::Security::Cryptography::*;
        match self {
            HashAlgorithm::Sha1 => BCRYPT_SHA1_ALGORITHM,
            HashAlgorithm::Sha256 => BCRYPT_SHA256_ALGORITHM,
            HashAlgorithm::Sha384 => BCRYPT_SHA384_ALGORITHM,
            HashAlgorithm::Sha512 => BCRYPT_SHA512_ALGORITHM,
        }
    }

    #[cfg(all(native, windows))]
    pub(crate) const fn output_size(self) -> usize {
        match self {
            HashAlgorithm::Sha1 => 20,
            HashAlgorithm::Sha256 => 32,
            HashAlgorithm::Sha384 => 48,
            HashAlgorithm::Sha512 => 64,
        }
    }

    /// Returns the hash algorithm named by a CryptoAPI RSA signature
    /// algorithm OID string (`szOID_RSA_*RSA`), or `None` if `oid` is null or
    /// names an unsupported algorithm.
    #[cfg(all(native, windows))]
    pub(crate) fn from_rsa_signature_oid(oid: windows::core::PSTR) -> Option<Self> {
        use windows::Win32::Security::Cryptography::szOID_RSA_SHA1RSA;
        use windows::Win32::Security::Cryptography::szOID_RSA_SHA256RSA;
        use windows::Win32::Security::Cryptography::szOID_RSA_SHA384RSA;
        use windows::Win32::Security::Cryptography::szOID_RSA_SHA512RSA;

        if oid.is_null() {
            return None;
        }
        // SAFETY: ASN.1-decoded OID strings from crypt32 are null-terminated,
        // as are the szOID_* constants.
        unsafe {
            let oid = oid.as_bytes();
            if oid == szOID_RSA_SHA256RSA.as_bytes() {
                Some(HashAlgorithm::Sha256)
            } else if oid == szOID_RSA_SHA384RSA.as_bytes() {
                Some(HashAlgorithm::Sha384)
            } else if oid == szOID_RSA_SHA512RSA.as_bytes() {
                Some(HashAlgorithm::Sha512)
            } else if oid == szOID_RSA_SHA1RSA.as_bytes() {
                Some(HashAlgorithm::Sha1)
            } else {
                None
            }
        }
    }

    /// Returns the Security.framework `SecKeyAlgorithm` constant for
    /// RSA PKCS#1 v1.5 message signing/verification with this hash.
    #[cfg(all(native, target_os = "macos"))]
    pub(crate) fn sec_key_alg_rsa_pkcs1(self) -> crate::mac::CFStringRef {
        #[link(name = "Security", kind = "framework")]
        unsafe extern "C" {
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePKCS1v15SHA1"]
            static SHA1: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePKCS1v15SHA256"]
            static SHA256: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePKCS1v15SHA384"]
            static SHA384: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePKCS1v15SHA512"]
            static SHA512: crate::mac::CFStringRef;
        }
        // SAFETY: extern statics are always valid.
        unsafe {
            match self {
                HashAlgorithm::Sha1 => SHA1,
                HashAlgorithm::Sha256 => SHA256,
                HashAlgorithm::Sha384 => SHA384,
                HashAlgorithm::Sha512 => SHA512,
            }
        }
    }

    /// Returns the Security.framework `SecKeyAlgorithm` constant for
    /// RSA-PSS message signing/verification with this hash.
    #[cfg(all(native, target_os = "macos"))]
    pub(crate) fn sec_key_alg_rsa_pss(self) -> crate::mac::CFStringRef {
        #[link(name = "Security", kind = "framework")]
        unsafe extern "C" {
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePSSSHA1"]
            static SHA1: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePSSSHA256"]
            static SHA256: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePSSSHA384"]
            static SHA384: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSASignatureMessagePSSSHA512"]
            static SHA512: crate::mac::CFStringRef;
        }
        // SAFETY: extern statics are always valid.
        unsafe {
            match self {
                HashAlgorithm::Sha1 => SHA1,
                HashAlgorithm::Sha256 => SHA256,
                HashAlgorithm::Sha384 => SHA384,
                HashAlgorithm::Sha512 => SHA512,
            }
        }
    }

    /// Returns the Security.framework `SecKeyAlgorithm` constant for
    /// RSA-OAEP encryption/decryption with this hash.
    #[cfg(all(native, target_os = "macos"))]
    pub(crate) fn sec_key_alg_rsa_oaep(self) -> crate::mac::CFStringRef {
        #[link(name = "Security", kind = "framework")]
        unsafe extern "C" {
            #[link_name = "kSecKeyAlgorithmRSAEncryptionOAEPSHA1"]
            static SHA1: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSAEncryptionOAEPSHA256"]
            static SHA256: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSAEncryptionOAEPSHA384"]
            static SHA384: crate::mac::CFStringRef;
            #[link_name = "kSecKeyAlgorithmRSAEncryptionOAEPSHA512"]
            static SHA512: crate::mac::CFStringRef;
        }
        // SAFETY: extern statics are always valid.
        unsafe {
            match self {
                HashAlgorithm::Sha1 => SHA1,
                HashAlgorithm::Sha256 => SHA256,
                HashAlgorithm::Sha384 => SHA384,
                HashAlgorithm::Sha512 => SHA512,
            }
        }
    }
}

#[cfg(any(symcrypt, rust, all(native, target_os = "macos")))]
impl TryFrom<der::asn1::ObjectIdentifier> for HashAlgorithm {
    type Error = der::Error;

    fn try_from(oid: der::asn1::ObjectIdentifier) -> Result<Self, Self::Error> {
        match oid {
            der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION => Ok(Self::Sha256),
            der::oid::db::rfc5912::SHA_384_WITH_RSA_ENCRYPTION => Ok(Self::Sha384),
            der::oid::db::rfc5912::SHA_512_WITH_RSA_ENCRYPTION => Ok(Self::Sha512),
            der::oid::db::rfc5912::SHA_1_WITH_RSA_ENCRYPTION => Ok(Self::Sha1),
            _ => Err(der::ErrorKind::OidUnknown { oid }.to_error()),
        }
    }
}

#[cfg(openssl)]
impl From<HashAlgorithm> for openssl::hash::MessageDigest {
    fn from(hash_algorithm: HashAlgorithm) -> Self {
        match hash_algorithm {
            HashAlgorithm::Sha1 => openssl::hash::MessageDigest::sha1(),
            HashAlgorithm::Sha256 => openssl::hash::MessageDigest::sha256(),
            HashAlgorithm::Sha384 => openssl::hash::MessageDigest::sha384(),
            HashAlgorithm::Sha512 => openssl::hash::MessageDigest::sha512(),
        }
    }
}

#[cfg(openssl)]
impl From<HashAlgorithm> for &'static openssl::md::MdRef {
    fn from(hash_algorithm: HashAlgorithm) -> Self {
        match hash_algorithm {
            HashAlgorithm::Sha1 => openssl::md::Md::sha1(),
            HashAlgorithm::Sha256 => openssl::md::Md::sha256(),
            HashAlgorithm::Sha384 => openssl::md::Md::sha384(),
            HashAlgorithm::Sha512 => openssl::md::Md::sha512(),
        }
    }
}

#[cfg(symcrypt)]
impl From<HashAlgorithm> for symcrypt::hash::HashAlgorithm {
    fn from(hash_algorithm: HashAlgorithm) -> Self {
        match hash_algorithm {
            HashAlgorithm::Sha1 => symcrypt::hash::HashAlgorithm::Sha1,
            HashAlgorithm::Sha256 => symcrypt::hash::HashAlgorithm::Sha256,
            HashAlgorithm::Sha384 => symcrypt::hash::HashAlgorithm::Sha384,
            HashAlgorithm::Sha512 => symcrypt::hash::HashAlgorithm::Sha512,
        }
    }
}
