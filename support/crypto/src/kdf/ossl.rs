// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::*;

pub struct KbkdfInner {
    inner: openssl_kdf::kdf::Kbkdf,
}

impl HashAlgorithm {
    fn to_message_digest(self) -> openssl::hash::MessageDigest {
        match self {
            HashAlgorithm::Sha256 => openssl::hash::MessageDigest::sha256(),
        }
    }
}

impl KbkdfInner {
    pub fn new(hash: HashAlgorithm, salt: Vec<u8>, key: Vec<u8>) -> Self {
        Self {
            inner: openssl_kdf::kdf::Kbkdf::new(hash.to_message_digest(), salt, key),
        }
    }

    pub fn set_context(&mut self, context: Vec<u8>) {
        self.inner.set_context(context);
    }

    pub fn set_mode(&mut self, mode: Mode) {
        let ossl_mode = match mode {
            Mode::Counter => openssl_kdf::kdf::Mode::Counter,
            Mode::Feedback => openssl_kdf::kdf::Mode::Feedback,
        };
        self.inner.set_mode(ossl_mode);
    }

    pub fn set_mac(&mut self, mac: Mac) {
        let ossl_mac = match mac {
            Mac::Hmac => openssl_kdf::kdf::Mac::Hmac,
            Mac::Cmac => openssl_kdf::kdf::Mac::Cmac,
        };
        self.inner.set_mac(ossl_mac);
    }

    pub fn set_seed(&mut self, seed: Vec<u8>) {
        self.inner.set_seed(seed);
    }

    pub fn set_l(&mut self, l: bool) {
        self.inner.set_l(l);
    }

    pub fn set_separator(&mut self, separator: bool) {
        self.inner.set_separator(separator);
    }
}

pub fn derive(kdf: KbkdfInner, output: &mut [u8]) -> Result<(), KdfError> {
    openssl_kdf::kdf::derive(kdf.inner, output).map_err(|e| {
        let stack = match e {
            openssl_kdf::kdf::KdfError::Ssl(stack) => stack,
            _ => openssl::error::ErrorStack::get(),
        };
        KdfError(crate::BackendError(stack, "KDF derivation"))
    })
}
