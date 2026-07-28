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

mod uefi_security_policy_tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn secure_boot_flag_off_passes_either_way() {
        let p = SivmPolicy::default();
        assert!(p.validate_secure_boot_enabled(false).is_ok());
        assert!(p.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn secure_boot_flag_on_passes_when_enabled() {
        let p = SivmPolicy {
            require_secure_boot: true,
            ..Default::default()
        };
        assert!(p.validate_secure_boot_enabled(true).is_ok());
    }

    #[test]
    fn secure_boot_flag_on_fails_when_disabled() {
        let p = SivmPolicy {
            require_secure_boot: true,
            ..Default::default()
        };
        let err = p.validate_secure_boot_enabled(false).unwrap_err();
        assert!(err.to_string().contains("secure boot"));
    }

    #[test]
    fn get_validated_uefi_json_fails_on_empty() {
        let p = SivmPolicy {
            custom_uefi_json: vec![],
            ..Default::default()
        };
        let err = p.get_validated_uefi_json().unwrap_err();
        assert!(err.to_string().contains("custom UEFI JSON"));
    }

    #[test]
    fn enforcement_rejects_unparseable_json() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: vec![0xFF, 0xFE],
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    /// Valid Replace-mode JSON with explicit PK/KEK/db/dbx.
    const REPLACE_JSON: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": {
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                },
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "db": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "dbx": [{
                    "type": "sha256",
                    "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
                }]
            }
        }
    }
}"#;

    /// Valid Replace-mode JSON with BCD hash custom variable.
    const REPLACE_JSON_WITH_BCD: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": {
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                },
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "db": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "dbx": [{
                    "type": "sha256",
                    "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
                }]
            },
            "BootConfigurationDataHash": {
                "guid": "Yd/ki8qT0hGqDQDgmAMrjA==",
                "attributes": "BwAAAA==",
                "value": "aGFzaHZhbHVl"
            }
        }
    }
}"#;

    /// Replace-mode JSON with a BootConfigurationDataHash under a wrong namespace GUID.
    const REPLACE_JSON_WITH_BCD_WRONG_GUID: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": {
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                },
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "db": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }],
                "dbx": [{
                    "type": "sha256",
                    "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="]
                }]
            },
            "BootConfigurationDataHash": {
                "guid": "vZr6d1kDTTK9YCj05494Sw==",
                "attributes": "BwAAAA==",
                "value": "aGFzaHZhbHVl"
            }
        }
    }
}"#;

    /// Append-mode JSON.
    const APPEND_JSON: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Append",
            "signatures": {
                "KEK": [{
                    "type": "x509",
                    "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="]
                }]
            }
        }
    }
}"#;

    #[test]
    fn enforcement_rejects_append_mode() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: APPEND_JSON.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("Replace mode"));
    }

    #[test]
    fn enforcement_passes_valid_replace_json() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            require_bcd_integrity: false,
            custom_uefi_json: REPLACE_JSON.to_vec(),
            ..Default::default()
        };
        assert!(p.validate_secure_boot_policy_enforcement().is_ok());
    }

    #[test]
    fn bcd_integrity_fails_when_hash_missing() {
        let p = SivmPolicy {
            require_bcd_integrity: true,
            custom_uefi_json: REPLACE_JSON.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("BootConfigurationDataHash"));
    }

    #[test]
    fn bcd_integrity_passes_when_hash_present() {
        let p = SivmPolicy {
            require_bcd_integrity: true,
            custom_uefi_json: REPLACE_JSON_WITH_BCD.to_vec(),
            ..Default::default()
        };
        assert!(p.validate_secure_boot_policy_enforcement().is_ok());
    }

    #[test]
    fn bcd_integrity_fails_when_hash_has_wrong_guid() {
        let p = SivmPolicy {
            require_bcd_integrity: true,
            custom_uefi_json: REPLACE_JSON_WITH_BCD_WRONG_GUID.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("BootConfigurationDataHash"));
    }

    /// Replace-mode JSON where PK relies on the template (Default).
    const REPLACE_JSON_PK_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "Default" },
                "KEK": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "db": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "dbx": [{ "type": "sha256", "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] }]
            }
        }
    }
}"#;

    /// Replace-mode JSON where KEK relies on the template (Default).
    const REPLACE_JSON_KEK_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] },
                "KEK": [{ "type": "Default" }],
                "db": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "dbx": [{ "type": "sha256", "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] }]
            }
        }
    }
}"#;

    /// Replace-mode JSON where db relies on the template (Default).
    const REPLACE_JSON_DB_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] },
                "KEK": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "db": [{ "type": "Default" }],
                "dbx": [{ "type": "sha256", "value": ["AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="] }]
            }
        }
    }
}"#;

    /// Replace-mode JSON where dbx relies on the template (Default).
    const REPLACE_JSON_DBX_DEFAULT: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {
                "PK": { "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] },
                "KEK": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "db": [{ "type": "x509", "value": ["ZmFrZV9jZXJ0X2RhdGFfZm9yX3Rlc3Q="] }],
                "dbx": [{ "type": "Default" }]
            }
        }
    }
}"#;

    #[test]
    fn enforcement_rejects_pk_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_PK_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("PK uses Default"));
    }

    #[test]
    fn enforcement_rejects_kek_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_KEK_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("KEK uses Default"));
    }

    #[test]
    fn enforcement_rejects_db_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_DB_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("db uses Default"));
    }

    #[test]
    fn enforcement_rejects_dbx_default_when_secure_boot_vars_required() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: REPLACE_JSON_DBX_DEFAULT.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("dbx uses Default"));
    }

    /// JSON with no uefiSettings section.
    const JSON_MISSING_UEFI_SETTINGS: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {}
}"#;

    /// Replace-mode JSON with an empty signatures object.
    const JSON_EMPTY_SIGNATURES: &[u8] = br#"{
    "type": "Microsoft.Compute/disks",
    "properties": {
        "uefiSettings": {
            "signatureMode": "Replace",
            "signatures": {}
        }
    }
}"#;

    #[test]
    fn enforcement_rejects_missing_uefi_settings() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: JSON_MISSING_UEFI_SETTINGS.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }

    #[test]
    fn enforcement_rejects_empty_signatures() {
        let p = SivmPolicy {
            require_secure_boot_vars: true,
            custom_uefi_json: JSON_EMPTY_SIGNATURES.to_vec(),
            ..Default::default()
        };
        let err = p.validate_secure_boot_policy_enforcement().unwrap_err();
        assert!(err.to_string().contains("failed to parse"));
    }
}
