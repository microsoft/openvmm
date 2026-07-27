// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification using the `x509-cert` RustCrypto crate.

use super::X509Error;
use super::X509PublicKey;
use der::Decode;
use der::Encode;
use x509_cert::Certificate;

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

/// Convert an error from constructing an RSA public key into an [`X509Error`].
#[cfg(symcrypt)]
fn rsa_to_x509(crate::rsa::RsaError(e): crate::rsa::RsaError) -> X509Error {
    X509Error(e)
}

/// Convert an error from constructing an RSA public key into an [`X509Error`].
/// The RustCrypto RSA error type does not fit the DER-based `X509Error`, so the
/// specific cause is folded into a generic failure (the RustCrypto backend is
/// test-only).
#[cfg(rust)]
fn rsa_to_x509(_e: crate::rsa::RsaError) -> X509Error {
    der_err(der::ErrorKind::Failed.into(), "constructing RSA public key")
}

pub(crate) struct X509CertificateInner(pub(crate) Certificate);

impl X509CertificateInner {
    pub fn from_der(data: &[u8]) -> Result<Self, X509Error> {
        let cert =
            Certificate::from_der(data).map_err(|e| der_err(e, "parsing DER certificate"))?;
        Ok(Self(cert))
    }

    pub fn public_key(&self) -> Result<X509PublicKey, X509Error> {
        const RSA_ENCRYPTION: der::asn1::ObjectIdentifier =
            der::asn1::ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
        #[cfg(symcrypt)]
        const ID_EC_PUBLIC_KEY: der::asn1::ObjectIdentifier =
            der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");

        let spki = self.0.tbs_certificate().subject_public_key_info();

        if spki.algorithm.oid == RSA_ENCRYPTION {
            let key = pkcs1::RsaPublicKey::from_der(spki.subject_public_key.raw_bytes())
                .map_err(|e| der_err(e, "parsing PKCS#1 RSA public key"))?;
            let rsa = crate::rsa::RsaPublicKey::from_components(
                key.modulus.as_bytes(),
                key.public_exponent.as_bytes(),
            )
            .map_err(rsa_to_x509)?;
            return Ok(X509PublicKey::Rsa(rsa));
        }

        // On the `symcrypt` backend we can also extract ECDSA keys; the `rust`
        // backend has no ECDSA support, so any non-RSA key is rejected below.
        #[cfg(symcrypt)]
        if spki.algorithm.oid == ID_EC_PUBLIC_KEY {
            // The SubjectPublicKey bit string of an EC key is the uncompressed
            // point `0x04 || Qx || Qy`. Require the `0x04` prefix (as the
            // OpenSSL and Windows backends do) and hand the raw coordinates to
            // the ECDSA parser, which validates the curve and imports the key.
            let point = spki.subject_public_key.raw_bytes();
            let qxqy = point.strip_prefix(&[0x04u8]).ok_or_else(|| {
                der_err(der::ErrorKind::Failed.into(), "parsing EC public key point")
            })?;
            let key = crate::ecdsa::EcdsaPublicKey::new(crate::ecdsa::EcdsaCurve::P384, qxqy)
                .map_err(|crate::ecdsa::EcdsaError(e)| X509Error(e))?;
            return Ok(X509PublicKey::Ecdsa(key));
        }

        Err(der_err(
            der::ErrorKind::Failed.into(),
            "extracting certificate public key",
        ))
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

    #[cfg(any(test, feature = "test_helpers"))]
    pub fn build_self_signed(
        key: &crate::rsa::RsaKeyPair,
        country: &str,
        state: &str,
        locality: &str,
        organization: &str,
        common_name: &str,
    ) -> anyhow::Result<Self> {
        use x509_cert::builder::Builder;

        let (builder, _pkcs1_der) = super::builder::self_signed_builder(
            key,
            country,
            state,
            locality,
            organization,
            common_name,
        )?;

        #[cfg(symcrypt)]
        let cert = builder.build(&super::builder::rsa_keypair_signer::RsaKeyPairSigner {
            key,
            pkcs1_der: _pkcs1_der,
        })?;
        #[cfg(rust)]
        let cert = builder.build(&rsa::pkcs1v15::SigningKey::<sha2::Sha256>::new(
            key.0.0.clone(),
        ))?;
        Ok(Self(cert))
    }

    pub fn issuer_dn(&self) -> Result<String, X509Error> {
        Ok(self.0.tbs_certificate().issuer().to_string())
    }

    pub fn serial_number(&self) -> Result<Vec<u8>, X509Error> {
        Ok(self.0.tbs_certificate().serial_number().as_bytes().to_vec())
    }

    pub fn subject_common_name(&self) -> Result<Option<String>, X509Error> {
        Ok(self
            .0
            .tbs_certificate()
            .subject()
            .common_name()
            .map_err(|e| der_err(e, "getting common_name"))?
            .map(|s| s.value().into_owned()))
    }
}
