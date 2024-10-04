// Copyright (C) Microsoft Corporation. All rights reserved.

//! This module includes the data types defined by Azure Key Vault (AKV) that
//! is used for parsing the response of the `KEY_RELEASE_REQUEST`.

use base64_serde::base64_serde_type;
use serde::Deserialize;
use serde::Serialize;

base64_serde_type!(Base64Url, base64::engine::general_purpose::URL_SAFE_NO_PAD);

/// The subset of standard JWT header.
#[derive(Debug, Deserialize, Serialize)]
pub struct AkvKeyReleaseJwtHeader {
    /// Indicate the signing algorithm, "none" indicates the JWT is unsigned (unsecured).
    pub alg: String,
    /// The certificate chain used to validate the signature if the JWT is signed (signed).
    #[serde(default)]
    pub x5c: Vec<String>,
}

/// The subset of the JWT payload format (in JSON) defined by Azure Key Vault (AKV) API version > 7.2
/// that includes the base64-url-encoded wrapped key JSON object.
/// The JWT payload JSON blob looks like
/// ```ignore
/// {
///    ..
///    "response": {
///        "key": {
///           ..
///           "key": {
///             ..
///             "key_hsm": <base64-url encoded wrapped key JSON object>
///           }
///       }
///    }
/// }
/// ```
#[derive(Debug, Deserialize, Serialize)]
pub struct AkvKeyReleaseJwtBody {
    /// JSON data
    pub response: AkvKeyReleaseResponse,
}

/// The subset of the `AkvKeyReleaseResponse` that includes the base64-url-encoded wrapped key JSON object.
#[derive(Debug, Deserialize, Serialize)]
pub struct AkvKeyReleaseResponse {
    /// JSON data
    pub key: AkvKeyReleaseKeyObject,
}

/// The subset of the `AkvKeyReleaseKeyObject` that includes the base64-url-encoded wrapped key JSON object.
#[derive(Debug, Deserialize, Serialize)]
pub struct AkvKeyReleaseKeyObject {
    /// JSON data
    pub key: AkvJwk,
}

/// The subset of the `AkvJwk` that holds the base64-url-encoded wrapped key JSON object in the `key_hsm`
/// field.
#[derive(Debug, Deserialize, Serialize)]
pub struct AkvJwk {
    /// JSON data with base64-url encoded value
    #[serde(with = "Base64Url")]
    pub key_hsm: Vec<u8>,
}

/// The subset of a JSON object (AKV API version 7.2) or decoded wrapped key JSON object (AKV API version > 7.2)
/// that holds the base64-url-encoded raw wrapped key blob in the `ciphertext` field.
/// The JSON object looks like
/// {
///    ..
///    "ciphertext": \<base64-url encoded raw wrapped key blob\>
/// }
#[derive(Deserialize, Serialize)]
pub struct AkvKeyReleaseKeyBlob {
    /// JSON data with base64-url encoded value
    #[serde(with = "Base64Url")]
    pub ciphertext: Vec<u8>,
}
