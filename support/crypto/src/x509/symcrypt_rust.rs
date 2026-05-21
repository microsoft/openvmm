// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification using the `x509-cert` RustCrypto crate.

use super::X509Error;
use core::str::FromStr;
use der::Decode;
use der::Encode;
use x509_cert::Certificate;
use x509_cert::builder::Builder;
use x509_cert::name::Name;

#[cfg(symcrypt)]
fn der_err(err: der::Error, op: &'static str) -> X509Error {
    X509Error(crate::BackendError::Der(err, op))
}

#[cfg(symcrypt)]
fn rsa_der_err(err: der::Error, op: &'static str) -> crate::rsa::RsaError {
    crate::rsa::RsaError(crate::BackendError::Der(err, op))
}

#[cfg(rust)]
fn der_err(err: der::Error, op: &'static str) -> X509Error {
    X509Error(err, op)
}

#[cfg(rust)]
fn rsa_der_err(err: der::Error, op: &'static str) -> crate::rsa::RsaError {
    crate::rsa::RsaError(rsa::Error::Pkcs1(pkcs1::Error::Asn1(err)), op)
}

pub struct X509CertificateInner(pub(crate) Certificate);

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let cert =
            Certificate::from_der(data).map_err(|e| der_err(e, "parsing DER certificate"))?;
        Ok(Self(cert))
    }

    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, crate::rsa::RsaError> {
        // Currently we only expect RSA public keys.
        // If someday we need to support other public key types, the return
        // type of this function will need to change.
        let key = pkcs1::RsaPublicKey::from_der(
            self.0
                .tbs_certificate()
                .subject_public_key_info()
                .subject_public_key
                .raw_bytes(),
        )
        .map_err(|e| rsa_der_err(e, "parsing PKCS#1 RSA public key"))?;
        crate::rsa::RsaPublicKey::from_components(
            key.modulus.as_bytes(),
            key.public_exponent.as_bytes(),
        )
    }

    pub fn verify(
        &self,
        issuer_public_key: &crate::rsa::RsaPublicKey,
    ) -> Result<bool, crate::rsa::RsaError> {
        let oid = self.0.signature_algorithm().oid;
        let hash = crate::HashAlgorithm::try_from(oid)
            .map_err(|e| rsa_der_err(e, "unrecognized signature algorithm OID"))?;

        let tbs_der = self
            .0
            .tbs_certificate()
            .to_der()
            .map_err(|e| rsa_der_err(e, "encoding TBS certificate"))?;
        let signature = self.0.signature().raw_bytes();

        issuer_public_key.pkcs1_verify(&tbs_der, signature, hash)
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> Result<bool, X509Error> {
        use x509_cert::ext::pkix::AuthorityKeyIdentifier;
        use x509_cert::ext::pkix::KeyUsage;
        use x509_cert::ext::pkix::SubjectKeyIdentifier;
        use x509_cert::ext::pkix::name::GeneralName;

        let issuer_tbs = self.0.tbs_certificate();
        let subject_tbs = subject.0.tbs_certificate();

        // The subject's issuer name must match the issuer's subject name.
        if subject_tbs.issuer() != issuer_tbs.subject() {
            return Ok(false);
        }

        // If this certificate has a KeyUsage extension, it must permit
        // signing other certificates.
        let ku = issuer_tbs
            .get_extension::<KeyUsage>()
            .map_err(|e| der_err(e, "parsing KeyUsage extension"))?;
        if let Some((_crit, ku)) = ku
            && !ku.key_cert_sign()
        {
            return Ok(false);
        }

        // If the subject carries an AuthorityKeyIdentifier, validate its
        // populated fields against this certificate (the candidate issuer).
        let akid = subject_tbs
            .get_extension::<AuthorityKeyIdentifier>()
            .map_err(|e| der_err(e, "parsing AuthorityKeyIdentifier extension"))?;
        if let Some((_crit, akid)) = akid {
            if let Some(akid_key_id) = &akid.key_identifier {
                let skid = issuer_tbs
                    .get_extension::<SubjectKeyIdentifier>()
                    .map_err(|e| der_err(e, "parsing SubjectKeyIdentifier extension"))?;
                match skid {
                    Some((_crit, ski)) => {
                        if akid_key_id != &ski.0 {
                            return Ok(false);
                        }
                    }
                    None => return Ok(false),
                }
            }
            if let Some(akid_serial) = &akid.authority_cert_serial_number {
                if akid_serial != issuer_tbs.serial_number() {
                    return Ok(false);
                }
            }
            if let Some(gens) = &akid.authority_cert_issuer {
                let mut has_dn = false;
                let has_matching_dn = gens.iter().any(|g| match g {
                    GeneralName::DirectoryName(dn) => {
                        has_dn = true;
                        dn == issuer_tbs.subject()
                    }
                    _ => false,
                });
                if has_dn && !has_matching_dn {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    pub fn to_der(&self) -> Result<Vec<u8>, X509Error> {
        self.0
            .to_der()
            .map_err(|e| der_err(e, "encoding certificate as DER"))
    }

    pub fn build_self_signed(
        key: &crate::rsa::RsaKeyPair,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> anyhow::Result<Self> {
        // Profile that produces a basic self-signed certificate with no
        // extensions and the same `Name` for both the subject and issuer.
        struct SelfSignedProfile {
            subject: Name,
        }

        impl x509_cert::builder::profile::BuilderProfile for SelfSignedProfile {
            fn get_issuer(&self, _subject: &Name) -> Name {
                self.subject.clone()
            }

            fn get_subject(&self) -> Name {
                self.subject.clone()
            }

            fn build_extensions(
                &self,
                _spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
                _issuer_spk: x509_cert::spki::SubjectPublicKeyInfoRef<'_>,
                _tbs: &x509_cert::TbsCertificate,
            ) -> x509_cert::builder::Result<Vec<x509_cert::ext::Extension>> {
                Ok(Vec::new())
            }
        }

        let name = Name::from_str(&format!(
            "CN={common_name},O={organization},L={locality},ST={state},C={country}"
        ))?;

        let modulus = key.modulus();
        let exponent = key.public_exponent();
        let pkcs1_pub = pkcs1::RsaPublicKey {
            modulus: der::asn1::UintRef::new(&modulus)?,
            public_exponent: der::asn1::UintRef::new(&exponent)?,
        };
        let pkcs1_der = pkcs1_pub.to_der()?;
        let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
            algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                oid: pkcs1::ALGORITHM_OID,
                parameters: None,
            },
            subject_public_key: der::asn1::BitString::from_bytes(&pkcs1_der)?,
        };

        let serial_number = x509_cert::serial_number::SerialNumber::from(1u32);
        let validity = x509_cert::time::Validity::new(
            der::asn1::GeneralizedTime::from_unix_duration(std::time::Duration::from_secs(0))?
                .into(),
            x509_cert::time::Time::INFINITY,
        );

        let profile = SelfSignedProfile { subject: name };
        let builder =
            x509_cert::builder::CertificateBuilder::new(profile, serial_number, validity, spki)?;

        #[cfg(symcrypt)]
        let cert = builder.build(&symcrypt_signer::SymCryptRsaSigner {
            key,
            spki_der: pkcs1_der,
        })?;
        #[cfg(rust)]
        let cert = builder.build(&rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(
            key.0.0.clone(),
        ))?;
        Ok(Self(cert))
    }
}

/// Adapters that implement the `signature` and `spki` traits required by
/// `x509_cert::builder::CertificateBuilder::build` on top of SymCrypt's
/// PKCS#1 v1.5 signing primitive.
#[cfg(symcrypt)]
mod symcrypt_signer {
    use der::Encode;

    pub struct SymCryptRsaSigner<'a> {
        pub key: &'a crate::rsa::RsaKeyPair,
        pub spki_der: Vec<u8>,
    }

    #[derive(Clone)]
    pub struct SymCryptRsaVerifyingKey(Vec<u8>);

    pub struct SymCryptRsaSignature(Vec<u8>);

    impl signature::Keypair for SymCryptRsaSigner<'_> {
        type VerifyingKey = SymCryptRsaVerifyingKey;

        fn verifying_key(&self) -> Self::VerifyingKey {
            SymCryptRsaVerifyingKey(self.spki_der.clone())
        }
    }

    impl x509_cert::spki::DynSignatureAlgorithmIdentifier for SymCryptRsaSigner<'_> {
        fn signature_algorithm_identifier(
            &self,
        ) -> x509_cert::spki::Result<x509_cert::spki::AlgorithmIdentifierOwned> {
            Ok(x509_cert::spki::AlgorithmIdentifierOwned {
                oid: der::oid::db::rfc5912::SHA_256_WITH_RSA_ENCRYPTION,
                parameters: None,
            })
        }
    }

    impl signature::Signer<SymCryptRsaSignature> for SymCryptRsaSigner<'_> {
        fn try_sign(&self, msg: &[u8]) -> Result<SymCryptRsaSignature, signature::Error> {
            self.key
                .pkcs1_sign(msg, crate::HashAlgorithm::Sha256)
                .map(SymCryptRsaSignature)
                .map_err(signature::Error::from_source)
        }
    }

    impl x509_cert::spki::EncodePublicKey for SymCryptRsaVerifyingKey {
        fn to_public_key_der(&self) -> x509_cert::spki::Result<der::Document> {
            // Wrap the PKCS#1 RSAPublicKey DER in a SubjectPublicKeyInfo and
            // encode the result as a DER document.
            let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
                algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                    oid: pkcs1::ALGORITHM_OID,
                    parameters: None,
                },
                subject_public_key: der::asn1::BitString::from_bytes(&self.0)?,
            };
            Ok(der::Document::try_from(spki.to_der()?)?)
        }
    }

    impl x509_cert::spki::SignatureBitStringEncoding for SymCryptRsaSignature {
        fn to_bitstring(&self) -> der::Result<der::asn1::BitString> {
            der::asn1::BitString::from_bytes(&self.0)
        }
    }
}
