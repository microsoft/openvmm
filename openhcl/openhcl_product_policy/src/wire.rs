// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-wire types for the measured product policy payload.
//!
//! Each [`ProductPolicy`] variant is a product identified by its
//! `#[mesh(N)]` tag. Tags are part of the measured wire format and
//! must not be reused. See `Guide/src/dev_guide/contrib/product_policy.md`
//! for the full onboarding guide.

extern crate alloc;

use alloc::vec::Vec;

use crate::cwcow::CwcowPolicy;

/// Measured product policy wire format. Each variant is a
/// product; mesh tags are part of the wire format and must not be
/// reused.
#[derive(mesh_protobuf::Protobuf, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "manifest", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "manifest",
    serde(rename_all = "snake_case", deny_unknown_fields)
)]
#[cfg_attr(feature = "inspect", derive(inspect::Inspect))]
#[cfg_attr(feature = "inspect", inspect(external_tag))]
#[mesh(package = "openhcl.product_policy")]
pub enum ProductPolicy {
    /// Confidential Windows Container on Windows.
    #[mesh(1)]
    Cwcow(CwcowPolicy),
}

impl ProductPolicy {
    /// Short tag identifying the product variant. Useful for logs and
    /// other diagnostic surfaces. New variants must extend this match.
    pub fn name(&self) -> &'static str {
        match self {
            ProductPolicy::Cwcow(_) => "cwcow",
        }
    }
}


/// Errors that may arise while decoding the inline measured
/// product policy bytes back into a [`ProductPolicy`].
#[derive(Debug)]
pub enum ProductPolicyDecodeError {
    /// The mesh_protobuf decoder rejected the bytes.
    Mesh(mesh_protobuf::Error),
}

impl core::fmt::Display for ProductPolicyDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mesh(_) => write!(f, "product policy mesh decode error"),
        }
    }
}

impl core::error::Error for ProductPolicyDecodeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Mesh(e) => Some(e),
        }
    }
}

/// Encode a [`ProductPolicy`] as `mesh_protobuf` bytes for
/// inclusion in the measured config region.
pub fn encode_product_policy(policy: &ProductPolicy) -> Vec<u8> {
    mesh_protobuf::encode(policy.clone())
}

/// Decode a non-empty [`ProductPolicy`] body. Callers must check
/// `product_policy_size != 0` first.
pub fn decode_product_policy(bytes: &[u8]) -> Result<ProductPolicy, ProductPolicyDecodeError> {
    mesh_protobuf::decode(bytes).map_err(ProductPolicyDecodeError::Mesh)
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::vec;

    fn sample_cwcow_policy() -> CwcowPolicy {
        CwcowPolicy {
            vmgs_read_only: true,
            require_secure_boot: true,
            require_secure_boot_vars: true,
            require_bcd_integrity: true,
            require_secure_avic: false,
            custom_uefi_json: vec![0xDE, 0xAD, 0xBE, 0xEF],
        }
    }

    #[test]
    fn product_policy_name_returns_variant_tag() {
        assert_eq!(
            ProductPolicy::Cwcow(CwcowPolicy::default()).name(),
            "cwcow"
        );
    }

    #[test]
    fn encode_decode_round_trip_default_cwcow() {
        let policy = ProductPolicy::Cwcow(CwcowPolicy::default());
        let bytes = encode_product_policy(&policy);
        let decoded = decode_product_policy(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn encode_decode_round_trip_nontrivial_cwcow() {
        let policy = ProductPolicy::Cwcow(sample_cwcow_policy());
        let bytes = encode_product_policy(&policy);
        let decoded = decode_product_policy(&bytes).unwrap();
        assert_eq!(decoded, policy);
    }

    #[test]
    fn decode_rejects_garbage() {
        let bad = [0xFFu8, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8];
        assert!(matches!(
            decode_product_policy(&bad),
            Err(ProductPolicyDecodeError::Mesh(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated() {
        let policy = ProductPolicy::Cwcow(sample_cwcow_policy());
        let mut bytes = encode_product_policy(&policy);
        bytes.pop();
        assert!(matches!(
            decode_product_policy(&bytes),
            Err(ProductPolicyDecodeError::Mesh(_))
        ));
    }

    #[cfg(feature = "manifest")]
    mod serde_tests {
        use super::*;

        fn from_json(s: &str) -> Result<ProductPolicy, serde_json::Error> {
            serde_json::from_str(s)
        }

        #[test]
        fn deserialize_cwcow_full() {
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": true,
                    "require_secure_boot": true,
                    "require_secure_boot_vars": true,
                    "require_bcd_integrity": true,
                    "require_secure_avic": true,
                    "custom_uefi_json": ""
                }
            }"#;
            let policy: ProductPolicy = from_json(json).unwrap();
            match policy {
                ProductPolicy::Cwcow(p) => {
                    assert!(p.vmgs_read_only);
                    assert!(p.require_secure_boot);
                    assert!(p.require_secure_boot_vars);
                    assert!(p.require_bcd_integrity);
                    assert!(p.require_secure_avic);
                    assert!(p.custom_uefi_json.is_empty());
                }
            }
        }

        #[test]
        fn deserialize_cwcow_missing_custom_uefi_json_is_an_error() {
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": true,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false
                }
            }"#;
            let err = from_json(json).unwrap_err();
            let msg = alloc::format!("{err}");
            assert!(
                msg.contains("custom_uefi_json"),
                "expected error to mention custom_uefi_json, got: {msg}"
            );
        }

        #[test]
        fn deserialize_cwcow_decodes_base64_custom_uefi_json() {
            let payload = b"{\"uefi\": \"sample\"}";
            let b64 = "eyJ1ZWZpIjogInNhbXBsZSJ9";
            let json = alloc::format!(
                r#"{{
                    "cwcow": {{
                        "vmgs_read_only": false,
                        "require_secure_boot": false,
                        "require_secure_boot_vars": false,
                        "require_bcd_integrity": false,
                        "require_secure_avic": false,
                        "custom_uefi_json": "{b64}"
                    }}
                }}"#
            );
            let policy: ProductPolicy = from_json(&json).unwrap();
            match policy {
                ProductPolicy::Cwcow(p) => assert_eq!(p.custom_uefi_json, payload.to_vec()),
            }
        }

        #[test]
        fn deserialize_cwcow_invalid_base64_is_an_error() {
            let json = r#"{
                "cwcow": {
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "custom_uefi_json": "***"
                }
            }"#;
            let err = from_json(json);
            assert!(err.is_err(), "expected base64 error, got: {err:?}");
        }

        #[test]
        fn json_round_trip_is_byte_identical() {
            let original = ProductPolicy::Cwcow(CwcowPolicy {
                vmgs_read_only: true,
                require_secure_boot: true,
                require_secure_boot_vars: true,
                require_bcd_integrity: true,
                require_secure_avic: true,
                custom_uefi_json: alloc::vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0x00, 0xFF],
            });
            let json = serde_json::to_string(&original).unwrap();
            let restored: ProductPolicy = from_json(&json).unwrap();
            assert_eq!(restored, original);
        }

        #[test]
        fn serialize_emits_custom_uefi_json_as_base64_string() {
            let policy = ProductPolicy::Cwcow(CwcowPolicy {
                custom_uefi_json: alloc::vec![b'A', b'B', b'C'],
                ..Default::default()
            });
            let json = serde_json::to_string(&policy).unwrap();
            assert!(
                json.contains("\"custom_uefi_json\":\"QUJD\""),
                "expected base64 string in JSON, got: {json}"
            );
        }

        #[test]
        fn deserialize_rejects_unknown_variant() {
            let err = from_json(r#"{"unknown_product":{}}"#);
            assert!(err.is_err());
        }

        #[test]
        fn deserialize_rejects_unknown_field() {
            let err = from_json(
                r#"{"cwcow":{
                    "vmgs_read_only": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "require_secure_avic": false,
                    "extra": 0
                }}"#,
            );
            assert!(err.is_err(), "expected error, got: {err:?}");
        }

        #[test]
        fn deserialize_rejects_pascal_case_variant() {
            let err = from_json(r#"{"Cwcow":{}}"#);
            assert!(err.is_err(), "expected error, got: {err:?}");
        }
    }
}
