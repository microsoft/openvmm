// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Hash algorithms and related utilities.

#![expect(deprecated)]
#![cfg(any(openssl, rust, symcrypt))]

/// Hash algorithm for RSA operations.
#[derive(Debug, Clone, Copy)]
pub enum HashAlgorithm {
    /// SHA-1
    #[deprecated(note = "SHA-1 is considered weak and should not be used for new applications")]
    Sha1,
    /// SHA-256
    Sha256,
}

impl HashAlgorithm {
    #[cfg(symcrypt)]
    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        match self {
            HashAlgorithm::Sha1 => symcrypt::hash::sha1(data).to_vec(),
            HashAlgorithm::Sha256 => symcrypt::hash::sha256(data).to_vec(),
        }
    }

    #[cfg(rust)]
    pub(crate) fn hash(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            HashAlgorithm::Sha1 => sha1::Sha1::digest(data).to_vec(),
            HashAlgorithm::Sha256 => sha2::Sha256::digest(data).to_vec(),
        }
    }
}

#[cfg(any(symcrypt, rust))]
impl TryFrom<der::asn1::ObjectIdentifier> for HashAlgorithm {
    type Error = der::Error;

    fn try_from(oid: der::asn1::ObjectIdentifier) -> Result<Self, Self::Error> {
        match oid {
            der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION => Ok(Self::Sha256),
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
        }
    }
}

#[cfg(openssl)]
impl From<HashAlgorithm> for &'static openssl::md::MdRef {
    fn from(hash_algorithm: HashAlgorithm) -> Self {
        match hash_algorithm {
            HashAlgorithm::Sha1 => openssl::md::Md::sha1(),
            HashAlgorithm::Sha256 => openssl::md::Md::sha256(),
        }
    }
}

#[cfg(symcrypt)]
impl From<HashAlgorithm> for symcrypt::hash::HashAlgorithm {
    fn from(hash_algorithm: HashAlgorithm) -> Self {
        match hash_algorithm {
            HashAlgorithm::Sha1 => symcrypt::hash::HashAlgorithm::Sha1,
            HashAlgorithm::Sha256 => symcrypt::hash::HashAlgorithm::Sha256,
        }
    }
}
