// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unit tests for the product policy codec, serde schema, and accessors.

extern crate alloc;

use super::*;
use crate::sivm::SivmPolicy;
use alloc::vec;

fn sample_sivm_policy() -> SivmPolicy {
    SivmPolicy {
        require_ephemeral_vmgs: true,
        require_secure_boot: true,
        require_secure_boot_vars: true,
        require_bcd_integrity: true,
        custom_uefi_json: vec![0xDE, 0xAD, 0xBE, 0xEF],
    }
}

#[test]
fn product_policy_name_returns_variant_tag() {
    assert_eq!(ProductPolicy::Sivm(SivmPolicy::default()).name(), "sivm");
}

#[test]
fn encode_decode_round_trip_nontrivial_sivm() {
    let policy = ProductPolicy::Sivm(sample_sivm_policy());
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
    let policy = ProductPolicy::Sivm(sample_sivm_policy());
    let mut bytes = encode_product_policy(&policy);
    bytes.pop();
    assert!(matches!(
        decode_product_policy(&bytes),
        Err(ProductPolicyDecodeError::Mesh(_))
    ));
}

#[test]
fn decode_rejects_bad_magic() {
    // A well-formed wrapper whose magic header does not match.
    let internal = ProductPolicyInternal {
        magic: 0,
        policy: ProductPolicy::Sivm(sample_sivm_policy()),
    };
    let bytes = mesh_protobuf::encode(internal);
    assert!(matches!(
        decode_product_policy(&bytes),
        Err(ProductPolicyDecodeError::BadMagic)
    ));
}

#[cfg(feature = "manifest")]
mod serde_tests {
    use super::*;

    fn from_json(s: &str) -> Result<ProductPolicy, serde_json::Error> {
        serde_json::from_str(s)
    }

    #[test]
    fn deserialize_sivm_full() {
        let json = r#"{
            "sivm": {
                "require_ephemeral_vmgs": true,
                "require_secure_boot": true,
                "require_secure_boot_vars": true,
                "require_bcd_integrity": true,
                "custom_uefi_json": ""
            }
        }"#;
        let policy: ProductPolicy = from_json(json).unwrap();
        match policy {
            ProductPolicy::Sivm(p) => {
                assert!(p.require_ephemeral_vmgs);
                assert!(p.require_secure_boot);
                assert!(p.require_secure_boot_vars);
                assert!(p.require_bcd_integrity);
                assert!(p.custom_uefi_json.is_empty());
            }
            _ => panic!("Expected Sivm policy"),
        }
    }

    #[test]
    fn deserialize_sivm_missing_custom_uefi_json_is_an_error() {
        let json = r#"{
            "sivm": {
                "require_ephemeral_vmgs": false,
                "require_secure_boot": true,
                "require_secure_boot_vars": false,
                "require_bcd_integrity": false
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
    fn deserialize_sivm_decodes_base64_custom_uefi_json() {
        let payload = b"{\"uefi\": \"sample\"}";
        let b64 = "eyJ1ZWZpIjogInNhbXBsZSJ9";
        let json = alloc::format!(
            r#"{{
                "sivm": {{
                    "require_ephemeral_vmgs": false,
                    "require_secure_boot": false,
                    "require_secure_boot_vars": false,
                    "require_bcd_integrity": false,
                    "custom_uefi_json": "{b64}"
                }}
            }}"#
        );
        let policy: ProductPolicy = from_json(&json).unwrap();
        match policy {
            ProductPolicy::Sivm(p) => assert_eq!(p.custom_uefi_json, payload.to_vec()),
            _ => panic!("Expected Sivm policy"),
        }
    }

    #[test]
    fn deserialize_sivm_invalid_base64_is_an_error() {
        let json = r#"{
            "sivm": {
                "require_ephemeral_vmgs": false,
                "require_secure_boot": false,
                "require_secure_boot_vars": false,
                "require_bcd_integrity": false,
                "custom_uefi_json": "***"
            }
        }"#;
        let err = from_json(json);
        assert!(err.is_err(), "expected base64 error, got: {err:?}");
    }

    #[test]
    fn json_round_trip_is_byte_identical() {
        let original = ProductPolicy::Sivm(SivmPolicy {
            require_ephemeral_vmgs: true,
            require_secure_boot: true,
            require_secure_boot_vars: true,
            require_bcd_integrity: true,
            custom_uefi_json: alloc::vec![0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0x00, 0xFF],
        });
        let json = serde_json::to_string(&original).unwrap();
        let restored: ProductPolicy = from_json(&json).unwrap();
        assert_eq!(restored, original);
    }

    #[test]
    fn serialize_emits_custom_uefi_json_as_base64_string() {
        let policy = ProductPolicy::Sivm(SivmPolicy {
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
            r#"{"sivm":{
                "require_ephemeral_vmgs": false,
                "require_secure_boot": false,
                "require_secure_boot_vars": false,
                "require_bcd_integrity": false,
                "extra": 0
            }}"#,
        );
        assert!(err.is_err(), "expected error, got: {err:?}");
    }

    #[test]
    fn deserialize_rejects_pascal_case_variant() {
        let err = from_json(r#"{"Sivm":{}}"#);
        assert!(err.is_err(), "expected error, got: {err:?}");
    }
}

mod measured_policy_tests {
    use super::*;

    fn measured(p: SivmPolicy) -> MeasuredProductPolicy {
        MeasuredProductPolicy::new(Some(ProductPolicy::Sivm(p)))
    }

    #[test]
    fn no_policy_yields_ok_none() {
        let r = MeasuredProductPolicy::new(None).sivm(|p| p.validate_secure_boot_enabled(false));
        assert!(matches!(r, Ok(None)));
    }

    #[test]
    fn passing_validation_yields_ok_some_unit() {
        let m = measured(SivmPolicy {
            require_secure_boot: true,
            ..Default::default()
        });
        assert!(matches!(
            m.sivm(|p| p.validate_secure_boot_enabled(true)),
            Ok(Some(()))
        ));
    }

    #[test]
    fn failing_validation_yields_err() {
        let m = measured(SivmPolicy {
            require_secure_boot: true,
            ..Default::default()
        });
        assert!(m.sivm(|p| p.validate_secure_boot_enabled(false)).is_err());
    }

    #[test]
    fn getter_via_ok_wrap() {
        let m = measured(SivmPolicy {
            custom_uefi_json: alloc::vec![b'h', b'i'],
            ..Default::default()
        });
        let json: Option<Vec<u8>> = m
            .sivm(|p| Ok(p.custom_uefi_json.clone()))
            .expect("no validation error");
        assert_eq!(json.as_deref(), Some(&b"hi"[..]));
    }
}
