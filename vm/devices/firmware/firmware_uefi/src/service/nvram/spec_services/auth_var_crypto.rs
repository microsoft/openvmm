// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cryptographic operations to validate authenticated variables

#![cfg(feature = "auth-var-verify-crypto")]

use super::ParsedAuthVar;
use thiserror::Error;
use uefi_nvram_specvars::signature_list;
use zerocopy::IntoBytes;

/// Errors that occur due to various formatting issues in the crypto objects.
#[derive(Debug, Error)]
pub enum FormatError {
    #[error("parsing signature list from auth_var_data")]
    SignatureList(#[from] signature_list::ParseError),
    #[error("decoding x509 cert from signature list")]
    SignatureListX509(#[source] openssl::error::ErrorStack),

    #[error("parsing auth var's pkcs7_data as pkcs#7 DER")]
    AuthVarPkcs7Der(#[source] openssl::error::ErrorStack),
    #[error("could not reconstruct signedData header for auth var's pkcs#7 data: {0}")]
    AuthVarPkcs7DerHeader(der::Error),
}

impl FormatError {
    /// Whether the error is due to malformed data in the signature lists
    pub fn key_var_error(&self) -> bool {
        match self {
            FormatError::SignatureList(_) | FormatError::SignatureListX509(_) => true,
            FormatError::AuthVarPkcs7Der(_) | FormatError::AuthVarPkcs7DerHeader(_) => false,
        }
    }
}

/// Authenticate the variable against the certs in the provided signature_lists,
/// returning `true` if the auth was successful.
pub fn authenticate_variable(
    signature_lists: &[u8],
    var: ParsedAuthVar<'_>,
) -> Result<bool, FormatError> {
    let ParsedAuthVar {
        name,
        vendor,
        attr,
        timestamp,
        pkcs7_data,
        var_data,
    } = var;

    // stage 1 - parse the pkcs7_data into an openssl Pkcs7 object
    let var_pkcs7 = match openssl::pkcs7::Pkcs7::from_der(pkcs7_data) {
        Ok(pkcs7) => pkcs7,
        Err(_) => {
            // From UEFI spec 8.2.2 Using the EFI_VARIABLE_AUTHENTICATION_2 descriptor
            //
            // > Construct a DER-encoded SignedData structure per PKCS#7 version 1.5
            // > (RFC 2315), which shall be supported **both with and without**
            // > a DER-encoded ContentInfo structure per PKCS#7 version 1.5 [..]
            //
            // (emphasis mine)
            //
            // Yes, you read that right.
            //
            // The UEFI spec explicitly allows _malformed_ PKCS#7 payloads that
            // are missing a ContentInfo header. _sigh_

            // stage 1.5 - if parsing fails the first time, construct an appropriate
            // ContentInfo header and retry parsing the payload as a PKCS#7 DER
            let buf = pkcs7_details::encapsulate_in_content_info(pkcs7_data)
                .map_err(FormatError::AuthVarPkcs7DerHeader)?;
            match openssl::pkcs7::Pkcs7::from_der(&buf) {
                Ok(pkcs7) => pkcs7,
                // ...but if that also fails, there's nothing else we can do
                Err(e) => return Err(FormatError::AuthVarPkcs7Der(e)),
            }
        }
    };

    // stage 2 - extract and parse all the x509 certs from the signature list(s)
    //           into openssl x509 objects
    let certs = {
        let mut parsed_certs = Vec::new();
        let lists = signature_list::ParseSignatureLists::new(signature_lists);
        for list in lists {
            let list = list?;
            // we only care about x509 certs in the signature lists
            if let signature_list::ParseSignatureList::X509(certs) = list {
                for cert in certs {
                    let cert = cert?;
                    let cert = openssl::x509::X509::from_der(&cert.data.0)
                        .map_err(FormatError::SignatureListX509)?;
                    parsed_certs.push(cert);
                }
            }
        }
        parsed_certs
    };

    // stage 3 - construct the "data to verify" buffer
    //
    // See bullet point 2. in UEFI spec 8.2.2
    let mut verify_buf = Vec::new();
    verify_buf.extend(name.as_bytes_without_nul());
    verify_buf.extend(vendor.as_bytes());
    verify_buf.extend(attr.as_bytes());
    verify_buf.extend(timestamp.as_bytes());
    verify_buf.extend(var_data);

    // stage 4 - package those raw certs into an openssl X509Store object
    let store = {
        let mut store = openssl::x509::store::X509StoreBuilder::new().unwrap();

        // unlike the HCL / worker process implementations, which manually
        // compare certs to perform the verification, we leverage openssl's
        // built-in functionality to do this for us.
        //
        // first, we throw all our trusted certs into a X509Store:
        for cert in certs {
            store.add_cert(cert).unwrap();
        }

        // then, we set some extra flags to work around the particular
        // idiosyncrasies of how these certs are constructed...

        // PARTIAL_CHAIN rationale: the certs in the EFI_SIGNATURE_LIST are not
        // root certs, and we don't have a full cert chain available. Instead,
        // we want to terminate the chain verification at whatever certs are
        // present from the EFI_SIGNATURE_LISTs.
        //
        // NO_CHECK_TIME rationale: when testing this feature, we noticed that
        // the UEFI signing key expired a long time ago. The existing
        // implementations didn't care about this, and allowed the verification
        // to succeed regardless.
        let store_flags = openssl::x509::verify::X509VerifyFlags::PARTIAL_CHAIN
            | openssl::x509::verify::X509VerifyFlags::NO_CHECK_TIME;
        store.set_flags(store_flags).unwrap();

        // X509Purpose::Any rationale: openssl expects the trusted certs to have
        // certain capabilities that ours do not. Omitting this call will result
        // in the verify operation failing with "Verify error:unsupported
        // certificate purpose"
        store
            .set_purpose(openssl::x509::X509PurposeId::ANY)
            .unwrap();

        store.build()
    };

    // stage 5 - actually perform the verification
    match var_pkcs7.verify(
        // `certs` should be nullable (i.e: represented using an optional).
        // This is an oversight in the openssl-rs API, so instead, we use an
        // empty stack...
        &openssl::stack::Stack::new().unwrap(),
        &store,
        Some(&verify_buf),
        None,
        openssl::pkcs7::Pkcs7Flags::empty(),
    ) {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::trace!(
                error = &e as &dyn std::error::Error,
                "could not verify auth var"
            );
            Ok(false)
        }
    }
}

mod pkcs7_details {
    use der::asn1::AnyRef;
    use der::asn1::ContextSpecific;
    use der::asn1::ObjectIdentifier;
    use der::Encode;
    use der::Sequence;
    use der::TagMode;
    use der::TagNumber;

    #[derive(Copy, Clone, Debug, Eq, PartialEq, Sequence)]
    struct ContentInfo<'a> {
        pub content_type: ObjectIdentifier,
        pub content: ContextSpecific<AnyRef<'a>>,
    }

    /// Construct a ASN.1 `ContentInfo` header with `ContentType = signedData`
    /// as specified by the PKCS#7 RFC2315.
    ///
    /// See https://datatracker.ietf.org/doc/html/rfc2315#section-7
    ///
    /// ```text
    /// ContentInfo ::= SEQUENCE {
    ///   contentType ContentType,
    ///   content
    ///     [0] EXPLICIT ANY DEFINED BY contentType OPTIONAL }
    /// ```
    pub fn encapsulate_in_content_info(content: &[u8]) -> der::Result<Vec<u8>> {
        // constant pulled from https://datatracker.ietf.org/doc/html/rfc2315#section-14
        const PKCS_7_SIGNED_DATA_OID: ObjectIdentifier =
            ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");

        let content_info = ContentInfo {
            content_type: PKCS_7_SIGNED_DATA_OID,
            content: ContextSpecific {
                tag_number: TagNumber::new(0),
                value: AnyRef::try_from(content)?,
                tag_mode: TagMode::Explicit,
            },
        };

        Encode::to_der(&content_info)
    }
}
