// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::JsonToTemplateError;
use crate::ParseJsonError;
use crate::parse_delta_json;
use crate::parse_template_json;
use base64::Engine;
use firmware_uefi_custom_vars::FinalVars;
use firmware_uefi_custom_vars::Signature;
use firmware_uefi_custom_vars::Signatures;
use firmware_uefi_custom_vars::delta::SignaturesDelta;
use serde_json::Value;
use serde_json::json;
use test_with_tracing::test;

fn encoded(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn document(mode: &str, signatures: Value) -> Value {
    json!({
        "type": "Microsoft.Compute/disks",
        "properties": {"uefiSettings": {
            "signatureMode": mode,
            "signatures": signatures
        }}
    })
}

fn template() -> Value {
    document(
        "Replace",
        json!({
            "PK": {"type": "x509", "value": [encoded(&[0, 0xff])]},
            "KEK": [{"type": "x509", "value": [encoded(&[1, 2, 3])]}],
            "db": [{"type": "sha256", "value": [encoded(&[0x5a; 32]), encoded(&[0xa5; 32])]}],
            "dbx": [{"type": "sha256", "value": [encoded(&[0x11; 32])]}],
            "MokList": [{"type": "x509", "value": [encoded(&[4, 5])]}],
            "MokListX": [{"type": "sha256", "value": [encoded(&[0x22; 32])]}]
        }),
    )
}

fn assert_certificates(signature: &Signature, expected: &[&[u8]]) {
    let Signature::X509(certificates) = signature else {
        panic!("expected X509 signature");
    };
    assert_eq!(
        certificates
            .iter()
            .map(|cert| cert.0.as_slice())
            .collect::<Vec<_>>(),
        expected
    );
}

fn assert_digests(signature: &Signature, expected: &[[u8; 32]]) {
    let Signature::Sha256(digests) = signature else {
        panic!("expected SHA256 signature");
    };
    assert_eq!(
        digests.iter().map(|digest| digest.0).collect::<Vec<_>>(),
        expected
    );
}

fn assert_template_signatures(sigs: &Signatures) {
    assert_certificates(&sigs.pk, &[&[0, 0xff]]);
    assert_eq!(sigs.kek.len(), 1);
    assert_certificates(&sigs.kek[0], &[&[1, 2, 3]]);
    assert_eq!(sigs.db.len(), 1);
    assert_digests(&sigs.db[0], &[[0x5a; 32], [0xa5; 32]]);
    assert_eq!(sigs.dbx.len(), 1);
    assert_digests(&sigs.dbx[0], &[[0x11; 32]]);
    assert_eq!(sigs.moklist.len(), 1);
    assert_certificates(&sigs.moklist[0], &[&[4, 5]]);
    assert_eq!(sigs.moklistx.len(), 1);
    assert_digests(&sigs.moklistx[0], &[[0x22; 32]]);
}

// Test: A fully defined template supplies every signature family and opaque vars.
// Expected: Final NVRAM data retains ordered certificate/hash bytes, GUID byte
//       order, Unicode names, custom payloads and little-endian short attributes.
#[test]
fn complete_template_and_custom_variable_bytes() {
    for (attributes, expected) in [
        (vec![0xa1], 0xa1),
        (vec![0xa1, 0xb2], 0xb2a1),
        (vec![0xa1, 0xb2, 0xc3], 0xc3b2a1),
        (vec![0xa1, 0xb2, 0xc3, 0xd4], 0xd4c3b2a1),
    ] {
        let mut doc = template();
        doc["properties"]["uefiSettings"]["Ownerβ"] = json!({
            "guid": encoded(&[
                0x33, 0x22, 0x11, 0, 0x55, 0x44, 0x77, 0x66,
                0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
            ]),
            "attributes": encoded(&attributes),
            "value": encoded(&[0, 0xff, 0x10]),
        });
        let base = parse_template_json(&serde_json::to_vec(&doc).unwrap()).unwrap();
        let vars = FinalVars::resolve(Some(base), None)
            .unwrap()
            .into_uefi_vars();
        let sigs = vars.signatures.unwrap();
        assert_template_signatures(&sigs);
        assert_eq!(vars.non_signature_vars.len(), 1);
        let (name, var) = &vars.non_signature_vars[0];
        assert_eq!(name, "Ownerβ");
        assert_eq!(
            var.guid,
            guid::guid!("00112233-4455-6677-8899-aabbccddeeff")
        );
        assert_eq!(var.attr, expected);
        assert_eq!(var.value, [0, 0xff, 0x10]);
    }
}

// Test: Append includes real signatures, explicit empty lists and omitted lists.
// Expected: Parsed presence survives, appended entries follow the base entries,
//       and neither omission nor an empty append erases an existing signature.
#[test]
fn append_preserves_presence_and_base_signatures() {
    let base = parse_template_json(&serde_json::to_vec(&template()).unwrap()).unwrap();
    let doc = document(
        "Append",
        json!({
            "KEK": [{"type": "x509", "value": [encoded(&[6, 7])]}],
            "db": [{"type": "sha256", "value": [encoded(&[0x33; 32])]}],
            "dbx": [],
            "MokList": [{"type": "x509", "value": [encoded(&[8, 9])]}],
            "MokListX": []
        }),
    );
    let delta = parse_delta_json(&serde_json::to_vec(&doc).unwrap()).unwrap();
    let SignaturesDelta::Append(sigs) = &delta.signatures else {
        panic!("expected append");
    };
    assert!(sigs.dbx.as_ref().unwrap().is_empty());
    assert!(sigs.moklistx.as_ref().unwrap().is_empty());
    let vars = FinalVars::resolve(Some(base), Some(delta))
        .unwrap()
        .into_uefi_vars();
    let sigs = vars.signatures.unwrap();
    assert_certificates(&sigs.pk, &[&[0, 0xff]]);
    assert_eq!(sigs.kek.len(), 2);
    assert_certificates(&sigs.kek[0], &[&[1, 2, 3]]);
    assert_certificates(&sigs.kek[1], &[&[6, 7]]);
    assert_eq!(sigs.db.len(), 2);
    assert_digests(&sigs.db[1], &[[0x33; 32]]);
    assert_eq!(sigs.dbx.len(), 1);
    assert_digests(&sigs.dbx[0], &[[0x11; 32]]);
    assert_eq!(sigs.moklist.len(), 2);
    assert_certificates(&sigs.moklist[1], &[&[8, 9]]);
    assert_eq!(sigs.moklistx.len(), 1);

    let delta =
        parse_delta_json(&serde_json::to_vec(&document("Append", json!({}))).unwrap()).unwrap();
    let SignaturesDelta::Append(sigs) = delta.signatures else {
        panic!("expected append");
    };
    assert!(sigs.kek.is_none() && sigs.db.is_none() && sigs.dbx.is_none());
    assert!(sigs.moklist.is_none() && sigs.moklistx.is_none());
    let mut doc = template();
    doc["properties"]["uefiSettings"]["signatures"]
        .as_object_mut()
        .unwrap()
        .remove("MokList");
    doc["properties"]["uefiSettings"]["signatures"]
        .as_object_mut()
        .unwrap()
        .remove("MokListX");
    let base = parse_template_json(&serde_json::to_vec(&doc).unwrap()).unwrap();
    assert!(base.signatures().unwrap().moklist.is_empty());
    assert!(base.signatures().unwrap().moklistx.is_empty());
}

// Test: A delta can select defaults, but templates must be fully defined.
//       Defaults mixed with real entries or append operations are invalid.
// Expected: Each signature family rejects ambiguous defaults; delta defaults are
//       resolved with the template's original bytes and entry counts, whereas
//       templates return the specific policy error.
#[test]
fn defaults_and_append_are_rejected_at_the_correct_boundary() {
    let base = parse_template_json(&serde_json::to_vec(&template()).unwrap()).unwrap();
    let default = json!({"type": "Default"});
    let explicit = json!({"type": "x509", "value": [encoded(&[1])]});
    for field in ["PK", "KEK", "db", "dbx", "MokList", "MokListX"] {
        let mut doc = template();
        doc["properties"]["uefiSettings"]["signatures"][field] = if field == "PK" {
            default.clone()
        } else {
            json!([default])
        };
        let bytes = serde_json::to_vec(&doc).unwrap();
        let delta = parse_delta_json(&bytes).unwrap();
        let vars = FinalVars::resolve(Some(base.clone()), Some(delta))
            .unwrap()
            .into_uefi_vars();
        assert_template_signatures(&vars.signatures.unwrap());
        assert!(matches!(
            parse_template_json(&bytes),
            Err(ParseJsonError::SemanticTemplate(
                JsonToTemplateError::CannotUseDefault
            ))
        ));
    }
    for field in ["KEK", "db", "dbx", "MokList", "MokListX"] {
        for (values, message) in [
            (
                json!([default, default]),
                "Default can only be used once per signature array",
            ),
            (
                json!([default, explicit]),
                "Default cannot be used alongside any other vars in the signature array",
            ),
            (
                json!([explicit, default]),
                "Default cannot be used alongside any other vars in the signature array",
            ),
        ] {
            let mut doc = template();
            doc["properties"]["uefiSettings"]["signatures"][field] = values;
            let error = parse_delta_json(&serde_json::to_vec(&doc).unwrap()).unwrap_err();
            let ParseJsonError::SemanticDelta(error) = error else {
                panic!("expected semantic delta error");
            };
            assert_eq!(error.to_string(), message);
        }
        let mut signatures = json!({});
        signatures[field] = json!([default]);
        let error = parse_delta_json(&serde_json::to_vec(&document("Append", signatures)).unwrap())
            .unwrap_err();
        let ParseJsonError::SemanticDelta(error) = error else {
            panic!("expected append default error");
        };
        assert_eq!(error.to_string(), "Default cannot be used alongside Append");
    }
    assert!(matches!(
        parse_template_json(&serde_json::to_vec(&document("Append", json!({}))).unwrap()),
        Err(ParseJsonError::SemanticTemplate(
            JsonToTemplateError::CannotUseAppend
        ))
    ));
}

// Test: Root identity and base64-encoded GUID, attributes and digest sizes are bad.
// Expected: Invalid wire encodings and structural lengths return JSON errors,
//       not a partial delta; valid neighboring sizes remain tested above.
#[test]
fn malformed_wire_fields_are_not_partial_deltas() {
    let mut doc = template();
    doc["type"] = json!("not-a-disk");
    assert!(matches!(
        parse_delta_json(&serde_json::to_vec(&doc).unwrap()),
        Err(ParseJsonError::Json(_))
    ));
    for length in [31, 33] {
        let mut doc = template();
        doc["properties"]["uefiSettings"]["signatures"]["db"][0]["value"] =
            json!([encoded(&vec![0; length])]);
        assert!(matches!(
            parse_delta_json(&serde_json::to_vec(&doc).unwrap()),
            Err(ParseJsonError::Json(_))
        ));
    }
    for (field, values) in [
        (
            "guid",
            vec![encoded(&[0; 15]), encoded(&[0; 17]), "%%%".into()],
        ),
        (
            "attributes",
            vec![encoded(&[]), encoded(&[0; 5]), "%%%".into()],
        ),
        ("value", vec!["%%%".into()]),
    ] {
        for value in values {
            let mut doc = template();
            let mut var = json!({"guid": encoded(&[0; 16]), "attributes": encoded(&[7]), "value": encoded(&[1])});
            var[field] = json!(value);
            doc["properties"]["uefiSettings"]["Invalid"] = var;
            assert!(matches!(
                parse_delta_json(&serde_json::to_vec(&doc).unwrap()),
                Err(ParseJsonError::Json(_))
            ));
        }
    }
    let mut doc = template();
    doc["properties"]["uefiSettings"]["signatures"]["db"][0]["value"] = json!(["%%%"]);
    assert!(matches!(
        parse_delta_json(&serde_json::to_vec(&doc).unwrap()),
        Err(ParseJsonError::Json(_))
    ));
}
