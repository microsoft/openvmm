// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Parsing and validation logic for Hyper-V UEFI custom nvram variables JSON
//! files.
//!
//! Depending on the type of JSON (template vs. user-defined custom vars), the
//! correspond Rust type will either be a
//! [`CustomVars`](firmware_uefi_custom_vars::CustomVars) or a
//! [`CustomVarsDelta`](firmware_uefi_custom_vars::delta::CustomVarsDelta).

#![forbid(unsafe_code)]

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseJsonError {
    #[error("malformed JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("semantic error when parsing delta: {0}")]
    SemanticDelta(#[from] convert::JsonToDeltaError),
    #[error("semantic error when parsing template: {0}")]
    SemanticTemplate(#[from] JsonToTemplateError),
}

#[derive(Debug, Error)]
pub enum JsonToTemplateError {
    #[error("template cannot use \"Default\" variable type")]
    CannotUseDefault,
    #[error("template cannot use \"append\" operations")]
    CannotUseAppend,
}

/// Parse a [`CustomVars`](firmware_uefi_custom_vars::CustomVars) from a
/// _fully-defined_ template UEFI custom nvram variables JSON file.
///
/// In this context, _fully-defined_ means that the JSON files cannot include
/// any "fallback" entries, nor can it encode any "append" operations to
/// existing variables.
pub fn load_template_from_json(
    data: &[u8],
) -> Result<firmware_uefi_custom_vars::CustomVars, ParseJsonError> {
    use firmware_uefi_custom_vars::delta::CustomVarsDelta;
    use firmware_uefi_custom_vars::delta::SignatureDelta;
    use firmware_uefi_custom_vars::delta::SignatureDeltaVec;
    use firmware_uefi_custom_vars::delta::SignaturesDelta;
    use firmware_uefi_custom_vars::CustomVars;
    use firmware_uefi_custom_vars::Signature;
    use firmware_uefi_custom_vars::Signatures;

    fn deny_default(sig: SignatureDelta) -> Result<Signature, JsonToTemplateError> {
        match sig {
            SignatureDelta::Sig(sig) => Ok(sig),
            SignatureDelta::Default => Err(JsonToTemplateError::CannotUseDefault),
        }
    }

    fn deny_default_vec(sigs: SignatureDeltaVec) -> Result<Vec<Signature>, JsonToTemplateError> {
        match sigs {
            SignatureDeltaVec::Sigs(sig) => Ok(sig),
            SignatureDeltaVec::Default => Err(JsonToTemplateError::CannotUseDefault),
        }
    }

    let CustomVarsDelta {
        signatures,
        custom_vars,
    } = load_delta_from_json(data)?;

    Ok(CustomVars {
        signatures: Some(match signatures {
            SignaturesDelta::Append(_) => panic!("hardcoded templates cannot use append"),
            SignaturesDelta::Replace(signatures) => Signatures {
                pk: deny_default(signatures.pk)?,
                kek: deny_default_vec(signatures.kek)?,
                db: deny_default_vec(signatures.db)?,
                dbx: deny_default_vec(signatures.dbx)?,
                moklist: signatures
                    .moklist
                    .map(deny_default_vec)
                    .transpose()?
                    .unwrap_or_default(),
                moklistx: signatures
                    .moklistx
                    .map(deny_default_vec)
                    .transpose()?
                    .unwrap_or_default(),
            },
        }),
        custom_vars,
    })
}

/// Parse a [`CustomVarsDelta`](firmware_uefi_custom_vars::delta::CustomVarsDelta) from a user
/// provided UEFI custom nvram variables JSON file.
pub fn load_delta_from_json(
    data: &[u8],
) -> Result<firmware_uefi_custom_vars::delta::CustomVarsDelta, ParseJsonError> {
    // syntax validation
    let json: json::JsonRoot = serde_json::from_slice(data)?;
    // semantic validation
    let delta = convert::json_to_delta(json)?;
    Ok(delta)
}

mod json {
    use guid::Guid;
    use serde::Deserialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Deserialize)]
    pub struct JsonRoot {
        /// we don't actually care about the specific `type` field - all we care
        /// about is that it passes `validate_root_type`
        #[serde(rename = "type", deserialize_with = "parse::validate_root_type")]
        pub _type: (),
        #[serde(rename = "properties")]
        pub properties: Properties,
    }

    #[derive(Debug, Deserialize)]
    pub struct Properties {
        #[serde(rename = "uefiSettings")]
        pub uefi_settings: UefiSettings,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "signatureMode", content = "signatures")]
    pub enum Signatures {
        #[serde(rename = "Append")]
        Append(SignaturesAppend),
        #[serde(rename = "Replace")]
        Replace(SignaturesReplace),
    }

    #[derive(Debug, Deserialize)]
    pub struct UefiSettings {
        #[serde(flatten)]
        pub signatures: Signatures,
        #[serde(flatten)]
        pub custom_vars: BTreeMap<String, CustomVar>,
    }

    #[derive(Debug, Deserialize)]
    pub struct CustomVar {
        #[serde(rename = "guid", deserialize_with = "parse::base64_guid")]
        pub guid: Guid,
        #[serde(rename = "attributes", deserialize_with = "parse::base64_u32")]
        pub attr: u32,
        #[serde(rename = "value", with = "serde_helpers::base64_vec")]
        pub value: Vec<u8>,
    }

    #[derive(Debug, Deserialize)]
    pub struct SignaturesAppend {
        #[serde(rename = "KEK")]
        pub kek: Option<Vec<Signature>>,
        #[serde(rename = "db")]
        pub db: Option<Vec<Signature>>,
        #[serde(rename = "dbx")]
        pub dbx: Option<Vec<Signature>>,
        #[serde(rename = "MokList")]
        pub moklist: Option<Vec<Signature>>,
        #[serde(rename = "MokListX")]
        pub moklistx: Option<Vec<Signature>>,
    }

    #[derive(Debug, Deserialize)]
    pub struct SignaturesReplace {
        #[serde(rename = "PK")]
        pub pk: Signature,
        #[serde(rename = "KEK")]
        pub kek: Vec<Signature>,
        #[serde(rename = "db")]
        pub db: Vec<Signature>,
        #[serde(rename = "dbx")]
        pub dbx: Vec<Signature>,
        #[serde(rename = "MokList")]
        pub moklist: Option<Vec<Signature>>,
        #[serde(rename = "MokListX")]
        pub moklistx: Option<Vec<Signature>>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type")]
    pub enum Signature {
        #[serde(rename = "x509")]
        X509 { value: Vec<X509Cert> },
        #[serde(rename = "sha256")]
        Sha256 { value: Vec<Sha256Digest> },
        /// "Default" will pull the value of the signature from the specified
        /// hardcoded template (and fail if one wasn't specified)
        ///
        /// It shouldn't be used in the hardcoded templates
        #[serde(rename = "Default")]
        Default,
    }

    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    pub struct X509Cert {
        #[serde(with = "serde_helpers::base64_vec")]
        pub data: Vec<u8>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    pub struct Sha256Digest {
        #[serde(deserialize_with = "parse::base64_u8_32")]
        pub data: [u8; 32],
    }

    mod parse {
        use base64::Engine;
        use guid::Guid;
        use serde::Deserialize;
        use serde::Deserializer;
        use zerocopy::FromBytes;

        pub fn validate_root_type<'de, D: Deserializer<'de>>(d: D) -> Result<(), D::Error> {
            let s: &str = Deserialize::deserialize(d)?;

            if s != "Microsoft.Compute/disks" {
                return Err(serde::de::Error::custom(
                    r#"root "type" must be "Microsoft.Compute/disks""#,
                ));
            }

            Ok(())
        }

        pub fn base64_u8_32<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
            let s: &str = Deserialize::deserialize(d)?;

            let v = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(serde::de::Error::custom)?;
            v.try_into().map_err(|v: Vec<_>| {
                serde::de::Error::custom(format!("expected 32 bytes. found {} bytes", v.len()))
            })
        }

        #[allow(clippy::many_single_char_names)]
        pub fn base64_u32<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
            let s: &str = Deserialize::deserialize(d)?;

            let v = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(serde::de::Error::custom)?;
            let v: [u8; 4] = match v.as_slice() {
                &[a] => [a, 0, 0, 0],
                &[a, b] => [a, b, 0, 0],
                &[a, b, c] => [a, b, c, 0],
                &[a, b, c, d] => [a, b, c, d],
                other => {
                    return Err(serde::de::Error::custom(format!(
                        "expected 4 bytes. found {} bytes",
                        other.len()
                    )))
                }
            };
            Ok(u32::from_le_bytes(v))
        }

        pub fn base64_guid<'de, D: Deserializer<'de>>(d: D) -> Result<Guid, D::Error> {
            let s: &str = Deserialize::deserialize(d)?;

            let v = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(serde::de::Error::custom)?;
            if v.len() != size_of::<Guid>() {
                return Err(serde::de::Error::custom(format!(
                    "expected {} bytes. found {} bytes",
                    size_of::<Guid>(),
                    v.len()
                )));
            }
            Ok(Guid::read_from_prefix(v.as_slice()).unwrap().0) // TODO: zerocopy: use-rest-of-range (https://github.com/microsoft/openvmm/issues/759)
        }
    }
}

mod convert {
    use super::json;
    use firmware_uefi_custom_vars as base;
    use firmware_uefi_custom_vars::delta;
    use thiserror::Error;

    #[derive(Debug, Error)]
    pub enum JsonToDeltaError {
        #[error("Default can only be used once per signature array")]
        MultipleDefault,
        #[error("Default cannot be used alongside any other vars in the signature array")]
        DefaultWithOtherVars,
        #[error("Default cannot be used alongside Append")]
        DefaultWithAppend,
    }

    fn validate_default(
        sigs: Vec<delta::SignatureDelta>,
    ) -> Result<delta::SignatureDeltaVec, JsonToDeltaError> {
        let num_sigs = sigs.len();
        let num_defaults = sigs
            .iter()
            .filter(|s| matches!(s, delta::SignatureDelta::Default))
            .count();

        // `.collect::<Option<Vec<_>>>()` will collapse a vec with _any_
        // Defaults into a `None`
        let sigs = sigs
            .into_iter()
            .map(|s| match s {
                delta::SignatureDelta::Default => None,
                delta::SignatureDelta::Sig(sig) => Some(sig),
            })
            .collect::<Option<Vec<_>>>();

        match sigs {
            Some(sigs) => Ok(delta::SignatureDeltaVec::Sigs(sigs)),
            None => {
                if num_defaults != 1 {
                    return Err(JsonToDeltaError::MultipleDefault);
                }

                if num_sigs != 1 {
                    return Err(JsonToDeltaError::DefaultWithOtherVars);
                }

                Ok(delta::SignatureDeltaVec::Default)
            }
        }
    }

    fn deny_default(sig_delta: json::Signature) -> Result<base::Signature, JsonToDeltaError> {
        let ret = match sig_delta {
            json::Signature::X509 { value } => {
                base::Signature::X509(value.into_iter().map(Into::into).collect())
            }
            json::Signature::Sha256 { value } => {
                base::Signature::Sha256(value.into_iter().map(Into::into).collect())
            }
            json::Signature::Default => return Err(JsonToDeltaError::DefaultWithAppend),
        };

        Ok(ret)
    }

    fn json_to_base_sig_vec(
        sigs: Vec<json::Signature>,
    ) -> Result<Vec<base::Signature>, JsonToDeltaError> {
        sigs.into_iter()
            .map(deny_default)
            .collect::<Result<Vec<_>, _>>()
    }

    fn json_to_delta_sig_vec(sigs: Vec<json::Signature>) -> Vec<delta::SignatureDelta> {
        sigs.into_iter().map(Into::into).collect()
    }

    /// Convert the JSON-specific data types into the format agnostic
    /// [`CustomVarsDelta`](delta::CustomVarsDelta) container.
    pub(super) fn json_to_delta(
        json: json::JsonRoot,
    ) -> Result<delta::CustomVarsDelta, JsonToDeltaError> {
        let custom_vars: Vec<(String, base::CustomVar)> = json
            .properties
            .uefi_settings
            .custom_vars
            .into_iter()
            .map(|(k, v)| (k, v.into()))
            .collect();

        let ret = match json.properties.uefi_settings.signatures {
            json::Signatures::Append(json::SignaturesAppend {
                kek,
                db,
                dbx,
                moklist,
                moklistx,
            }) => delta::CustomVarsDelta {
                signatures: delta::SignaturesDelta::Append(delta::SignaturesAppend {
                    kek: kek.map(json_to_base_sig_vec).transpose()?,
                    db: db.map(json_to_base_sig_vec).transpose()?,
                    dbx: dbx.map(json_to_base_sig_vec).transpose()?,
                    moklist: moklist.map(json_to_base_sig_vec).transpose()?,
                    moklistx: moklistx.map(json_to_base_sig_vec).transpose()?,
                }),
                custom_vars,
            },
            json::Signatures::Replace(json::SignaturesReplace {
                pk,
                kek,
                db,
                dbx,
                moklist,
                moklistx,
            }) => delta::CustomVarsDelta {
                signatures: delta::SignaturesDelta::Replace(delta::SignaturesReplace {
                    pk: pk.into(),
                    kek: validate_default(json_to_delta_sig_vec(kek))?,
                    db: validate_default(json_to_delta_sig_vec(db))?,
                    dbx: validate_default(json_to_delta_sig_vec(dbx))?,
                    moklist: moklist
                        .map(|sigs| validate_default(json_to_delta_sig_vec(sigs)))
                        .transpose()?,
                    moklistx: moklistx
                        .map(|sigs| validate_default(json_to_delta_sig_vec(sigs)))
                        .transpose()?,
                }),
                custom_vars,
            },
        };

        Ok(ret)
    }

    impl From<json::CustomVar> for base::CustomVar {
        fn from(json_custom_var: json::CustomVar) -> base::CustomVar {
            base::CustomVar {
                guid: json_custom_var.guid,
                attr: json_custom_var.attr,
                value: json_custom_var.value,
            }
        }
    }

    impl From<json::Signature> for delta::SignatureDelta {
        fn from(json_signature: json::Signature) -> delta::SignatureDelta {
            match json_signature {
                json::Signature::X509 { value } => delta::SignatureDelta::Sig(
                    base::Signature::X509(value.into_iter().map(Into::into).collect()),
                ),
                json::Signature::Sha256 { value } => delta::SignatureDelta::Sig(
                    base::Signature::Sha256(value.into_iter().map(Into::into).collect()),
                ),
                json::Signature::Default => delta::SignatureDelta::Default,
            }
        }
    }

    impl From<json::Sha256Digest> for base::Sha256Digest {
        fn from(json_digest: json::Sha256Digest) -> base::Sha256Digest {
            base::Sha256Digest(json_digest.data)
        }
    }

    impl From<json::X509Cert> for base::X509Cert {
        fn from(json_cert: json::X509Cert) -> base::X509Cert {
            base::X509Cert(json_cert.data)
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const APPEND: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Append",
                "signatures": {
                    "db": [{
                        "type": "sha256",
                        "value": ["i4HHnT7SW4zUdcQ7LyvOyfVOTr9BxCWWoP/W3lqGE0Q=",
                                "ELLxN/fAUzttWO5WW3SOdJ4a99OAWy56o2ga46W1Efc="]
                    }],
                    "dbx": [{
                        "type": "sha256",
                        "value": ["Bkk5Ef7VzGk3M7FLY/MyHyl+ygAF1P8ju9HBSl53Cak=",
                                "h6/eEfJXfmqGLZWr/ekVjhQAiG3OlGw6Yi4MAbBjpqc="]
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn append() {
        let data = serde_json::from_str::<json::JsonRoot>(APPEND);
        let _ = data.unwrap();
    }

    const APPEND_DEFAULT: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Append",
                "signatures": {
                    "db": [{
                        "type": "Default",
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn append_default() {
        let data = serde_json::from_str::<json::JsonRoot>(APPEND_DEFAULT);
        assert!(data.is_err())
    }

    const REPLACE_MALFORMED_DB: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Replace",
                "signatures": {
                    "PK": {
                        "type": "x509",
                        "value": ["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"]
                    },
                    "KEK": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "db": [{
                        "type": "x509",
                        "value": ["InvalidKey"]
                    }],
                    "dbx": [{
                        "type": "Default"
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn replace_malformed_db() {
        let data = serde_json::from_str::<json::JsonRoot>(REPLACE_MALFORMED_DB);
        assert!(data.is_err())
    }

    const REPLACE_MISSING_SIGNATURE_MODE: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatures": {
                    "PK": {
                        "type": "x509",
                        "value": ["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"]
                    },
                    "KEK": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "dbx": [{
                        "type": "Default"
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn replace_missing_signature_mode() {
        let data = serde_json::from_str::<json::JsonRoot>(REPLACE_MISSING_SIGNATURE_MODE);
        assert!(data.is_err())
    }

    const REPLACE_MULTI_DEFAULT: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Replace",
                "signatures": {
                    "PK": {
                        "type": "x509",
                        "value": ["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"]
                    },
                    "KEK": [{
                        "type": "Default"
                    }, {
                        "type": "Default"
                    }],
                    "db": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "dbx": [{
                        "type": "Default"
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn replace_multi_default() {
        // serde parses this fine
        let data = serde_json::from_str::<json::JsonRoot>(REPLACE_MULTI_DEFAULT).unwrap();
        // but semantic checks will fail
        let data = convert::json_to_delta(data);
        assert!(data.is_err())
    }

    const REPLACE_MIXED_DEFAULT: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Replace",
                "signatures": {
                    "PK": {
                        "type": "x509",
                        "value": ["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"]
                    },
                    "KEK": [{
                        "type": "Default"
                    }, {
                        "type": "x509",
                        "value": ["Jw=="]
                    }],
                    "db": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "dbx": [{
                        "type": "Default"
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn replace_mixed_default() {
        // serde parses this fine
        let data = serde_json::from_str::<json::JsonRoot>(REPLACE_MIXED_DEFAULT).unwrap();
        // but semantic checks will fail
        let data = convert::json_to_delta(data);
        assert!(data.is_err())
    }

    const REPLACE: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Replace",
                "signatures": {
                    "PK": {
                        "type": "x509",
                        "value": ["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"]
                    },
                    "KEK": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "db": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "dbx": [{
                        "type": "Default"
                    }]
                }
            }
        }
    }
    "#;

    #[test]
    fn replace() {
        let data = serde_json::from_str::<json::JsonRoot>(REPLACE);
        let _ = data.unwrap();
    }

    // BAD_DB is a semantic test that requires performing signature validation
    // (which isn't done as part of the basic JSON parsing + validation)

    // const BAD_DB: &str = r#"
    // {
    //     "type": "Microsoft.Compute/disks",

    //     "properties": {
    //         "uefiSettings" : {
    //             "signatureMode": "Replace",
    //             "signatures": {
    //                 "PK": {
    //                     "type": "x509",
    //                     "value": ["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"]
    //                 },
    //                 "KEK": [{
    //                     "type": "x509",
    //                     "value": ["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"]
    //                 }]
    //             },

    //             "db": {
    //                 "guid":"y7IZ1zo9lkWjvNrQDmdlbw==",
    //                 "attributes":"Jw==",
    //                 "value":"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"
    //             },

    //             "dbx": {
    //                 "guid":"y7IZ1zo9lkWjvNrQDmdlbw==",
    //                 "attributes":"Jw==",
    //                 "value":"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"
    //             }
    //         }
    //     }
    // }
    // "#;

    // #[test]
    // fn bad_db() {
    //     let data = serde_json::from_str::<json::JsonRoot>(BAD_DB);
    //     let _ = data.unwrap();
    // }

    const PRIVATE_VARS: &str = r#"
    {
        "type": "Microsoft.Compute/disks",

        "properties": {
            "uefiSettings" : {
                "signatureMode": "Replace",
                "signatures": {
                    "PK": {
                        "type": "x509",
                        "value": ["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"]
                    },
                    "KEK": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "db": [{
                        "type": "x509",
                        "value": ["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"]
                    }],
                    "dbx": [{
                        "type": "Default"
                    }]
                },

                "Var1": {
                    "guid":"Yd/ki8qT0hGqDQDgmAMrjA==",
                    "attributes":"Bw==",
                    "value":"VGVzdCBWYXJpYWJsZSAx"
                },

                "Var2": {
                    "guid":"Yd/ki8qT0hGqDQDgmAMrjA==",
                    "attributes":"Jw==",
                    "value":"VGVzdCBWYXJpYWJsZSAy"
                }
            }
        }
    }
    "#;

    #[test]
    fn private_vars() {
        let data = serde_json::from_str::<json::JsonRoot>(PRIVATE_VARS);
        let data = data.unwrap();
        match data.properties.uefi_settings.custom_vars.get("Var1") {
            Some(v) => assert_eq!(v.attr, 7),
            None => panic!(),
        }
        match data.properties.uefi_settings.custom_vars.get("Var2") {
            Some(v) => assert_eq!(v.attr, 39),
            None => panic!(),
        }
    }
}
