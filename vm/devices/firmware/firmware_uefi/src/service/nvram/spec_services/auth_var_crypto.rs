// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Cryptographic operations to validate authenticated variables

#![cfg(feature = "auth-var-verify-crypto")]

use super::ParsedAuthVar;
use thiserror::Error;
use uefi_nvram_specvars::signature_list;

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
    _signature_lists: &[u8],
    _var: ParsedAuthVar<'_>,
) -> Result<bool, FormatError> {
    panic!("We did it!");
}

#[allow(dead_code)]
mod pkcs7_details {
    use der::Encode;
    use der::Sequence;
    use der::TagMode;
    use der::TagNumber;
    use der::asn1::AnyRef;
    use der::asn1::ContextSpecific;
    use der::asn1::ObjectIdentifier;

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
