// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! X.509 certificate parsing and verification using the `x509-cert` RustCrypto crate.

use super::X509Error;
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

pub(crate) struct X509CertificateInner(pub(crate) Certificate);

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
