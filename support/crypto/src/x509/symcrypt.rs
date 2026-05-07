// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::X509Error;
use der::Decode;
use der::Encode;
use rsa::pkcs1v15::RsaSignatureAssociatedOid;
use symcrypt::rsa::RsaKeyUsage;
use x509_cert::Certificate;

fn err(err: symcrypt::errors::SymCryptError, op: &'static str) -> X509Error {
    X509Error(crate::BackendError::SymCrypt(err, op))
}

fn der_err(err: der::Error, op: &'static str) -> X509Error {
    X509Error(crate::BackendError::Der(err, op))
}

pub struct X509CertificateInner(Certificate);

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let cert =
            Certificate::from_der(data).map_err(|e| der_err(e, "parsing DER certificate"))?;
        Ok(Self(cert))
    }

    pub fn public_key(&self) -> Result<crate::rsa::RsaPublicKey, X509Error> {
        let parsed = ::rsa::pkcs1::RsaPublicKey::from_der(
            self.0
                .tbs_certificate()
                .subject_public_key_info()
                .subject_public_key
                .raw_bytes(),
        )
        .map_err(|e| der_err(e, "parsing PKCS#1 RSA public key"))?;
        let key = ::symcrypt::rsa::RsaKey::set_public_key(
            parsed.modulus.as_bytes(),
            parsed.public_exponent.as_bytes(),
            RsaKeyUsage::SignAndEncrypt,
        )
        .map_err(|e| err(e, "constructing RSA public key"))?;
        Ok(crate::rsa::RsaPublicKey(
            crate::rsa::symcrypt::RsaPublicKeyInner(key),
        ))
    }

    pub fn verify(&self, issuer_public_key: &crate::rsa::RsaPublicKey) -> Result<bool, X509Error> {
        let oid = self.0.signature_algorithm().oid;
        let hash = match oid {
            ::rsa::sha2::Sha256::OID => symcrypt::hash::HashAlgorithm::Sha256,
            _ => {
                return Err(der_err(
                    der::ErrorKind::OidUnknown { oid }.to_error(),
                    "unrecognized signature algorithm OID",
                ));
            }
        };

        let tbs_der = self
            .0
            .tbs_certificate()
            .to_der()
            .map_err(|e| der_err(e, "encoding TBS certificate"))?;
        let signature = self.0.signature().raw_bytes();

        issuer_public_key
            .0
            .0
            .pkcs1_verify(&tbs_der, signature, hash)
            .map_err(|e| err(e, "verifying certificate signature"))?;
        Ok(true)
    }

    pub fn issued(&self, subject: &X509CertificateInner) -> bool {
        self.0.tbs_certificate().subject() == subject.0.tbs_certificate().issuer()
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
        use core::str::FromStr;
        use x509_cert::builder::Builder;
        use x509_cert::name::Name;

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
        let pkcs1_pub = ::rsa::pkcs1::RsaPublicKey {
            modulus: ::der::asn1::UintRef::new(&modulus)?,
            public_exponent: ::der::asn1::UintRef::new(&exponent)?,
        };
        let pkcs1_der = pkcs1_pub.to_der()?;
        let spki = x509_cert::spki::SubjectPublicKeyInfoOwned {
            algorithm: x509_cert::spki::AlgorithmIdentifierOwned {
                oid: ::rsa::pkcs1::ALGORITHM_OID,
                parameters: Some(::der::asn1::Any::null()),
            },
            subject_public_key: ::der::asn1::BitString::from_bytes(&pkcs1_der)?,
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

        let blob = key.0.0.export_key_pair_blob()?;
        let priv_key = ::rsa::RsaPrivateKey::from_components(
            ::rsa::BoxedUint::from_be_slice_vartime(&blob.modulus),
            ::rsa::BoxedUint::from_be_slice_vartime(&blob.pub_exp),
            ::rsa::BoxedUint::from_be_slice_vartime(&blob.private_exp),
            vec![
                ::rsa::BoxedUint::from_be_slice_vartime(&blob.p),
                ::rsa::BoxedUint::from_be_slice_vartime(&blob.q),
            ],
        )?;
        let signer = ::rsa::pkcs1v15::SigningKey::<::rsa::sha2::Sha256>::new(priv_key);

        let cert = builder.build(&signer)?;
        Ok(Self(cert))
    }
}
