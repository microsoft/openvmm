// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![no_main]
#![expect(missing_docs)]
#![cfg(all(target_os = "linux", target_env = "gnu"))]

use arbitrary::Arbitrary;
use crypto::pkcs7::Pkcs7SignedData;
use crypto::rsa::RsaKeyPair;
use crypto::x509::X509Certificate;
use firmware_uefi::platform::nvram::EFI_TIME;
use firmware_uefi::service::nvram::spec_services::ParsedAuthVar;
use firmware_uefi::service::nvram::spec_services::auth_var_crypto;
use guid::Guid;
use std::borrow::Cow;
use ucs2::Ucs2LeVec;
use uefi_nvram_specvars::signature_list::SignatureData;
use uefi_nvram_specvars::signature_list::SignatureList;
use xtask_fuzz::fuzz_target;
use zerocopy::FromBytes;

#[derive(Debug, Arbitrary)]
struct AuthVarInput {
    name: String,
    vendor: [u8; 16],
    attr: u32,
    timestamp: [u8; 16],
    var_data: Vec<u8>,
}

#[derive(Debug, Arbitrary)]
enum FuzzInput {
    Random {
        var: AuthVarInput,
        signature_lists: Vec<u8>,
        pkcs7_data: Vec<u8>,
    },
    ParsePkcs7 {
        var: AuthVarInput,
        signature_lists: Vec<u8>,
        input: Vec<u8>,
    },
    ParsePkcs7ParseSignatureLists {
        var: AuthVarInput,
        input: Vec<u8>,
    },
    ParsePkcs7ValidSignatureLists {
        var: AuthVarInput,
        input: Vec<u8>,
    },
    MatchingPkcs7AndSignatureLists {
        var: AuthVarInput,
        input: Vec<u8>,
    },
}

fn do_fuzz(input: FuzzInput) {
    let (var, signature_lists, pkcs7_data) = match input {
        FuzzInput::Random {
            var,
            signature_lists,
            pkcs7_data,
        } => (var, signature_lists, pkcs7_data),
        FuzzInput::ParsePkcs7 {
            var,
            signature_lists,
            input,
        } => (var, signature_lists, test_pkcs7_data(&input)),
        FuzzInput::ParsePkcs7ParseSignatureLists { var, input } => {
            (var, test_signature_lists(), test_pkcs7_data(&input))
        }
        FuzzInput::ParsePkcs7ValidSignatureLists { var, input } => {
            (var, test_valid_signature_lists(), test_pkcs7_data(&input))
        }
        FuzzInput::MatchingPkcs7AndSignatureLists { var, input } => {
            let pkey = test_pkey();
            let signcert = test_x509(&pkey);
            let pkcs7 = Pkcs7SignedData::sign(&signcert, &pkey, &input).unwrap();
            let signature_lists = test_signature_list_from_x509(&signcert);
            (var, signature_lists, pkcs7.to_der().unwrap())
        }
    };

    let AuthVarInput {
        name,
        vendor,
        attr,
        timestamp,
        var_data,
    } = var;

    let var = ParsedAuthVar {
        name: &Ucs2LeVec::from(name),
        vendor: Guid::read_from_bytes(&vendor).unwrap(),
        attr,
        timestamp: EFI_TIME::read_from_bytes(&timestamp).unwrap(),
        pkcs7_data: &pkcs7_data,
        var_data: &var_data,
    };

    _ = auth_var_crypto::authenticate_variable(&signature_lists, var);
}

fuzz_target!(|input: FuzzInput| {
    xtask_fuzz::init_tracing_if_repro();
    do_fuzz(input)
});

const OWNER_1: Guid = Guid {
    data1: 1,
    data2: 0,
    data3: 0,
    data4: [0, 0, 0, 0, 0, 0, 0, 0],
};

const OWNER_2: Guid = Guid {
    data1: 2,
    data2: 0,
    data3: 0,
    data4: [0, 0, 0, 0, 0, 0, 0, 0],
};

fn test_signature_lists() -> Vec<u8> {
    let lists = vec![
        SignatureList::Sha256(vec![
            SignatureData::new_sha256(OWNER_1, Cow::Owned([0; 32])),
            SignatureData::new_sha256(OWNER_2, Cow::Owned([1; 32])),
            SignatureData::new_sha256(OWNER_1, Cow::Owned([2; 32])),
        ]),
        SignatureList::X509(SignatureData::new_x509(
            OWNER_2,
            b"some cert data"[..].into(),
        )),
        SignatureList::Sha256(vec![
            SignatureData::new_sha256(OWNER_1, Cow::Owned([0; 32])),
            SignatureData::new_sha256(OWNER_2, Cow::Owned([1; 32])),
        ]),
        SignatureList::X509(SignatureData::new_x509(
            OWNER_1,
            b"more cert data"[..].into(),
        )),
    ];
    let mut buf = Vec::new();
    for l in &lists {
        l.extend_as_spec_signature_list(&mut buf)
    }
    buf
}

const VALID_OWNER: Guid = guid::guid!("77fa9abd-0359-4d32-bd60-28f4e78f784b");

fn test_valid_signature_lists() -> Vec<u8> {
    let lists = vec![
        SignatureList::X509(SignatureData::new_x509(
            VALID_OWNER,
            Cow::Owned(include_bytes!("signature1.bin").to_vec()),
        )),
        SignatureList::X509(SignatureData::new_x509(
            VALID_OWNER,
            Cow::Owned(include_bytes!("signature2.bin").to_vec()),
        )),
    ];
    let mut buf = Vec::new();
    for l in &lists {
        l.extend_as_spec_signature_list(&mut buf)
    }
    buf
}

fn test_signature_list_from_x509(signcert: &X509Certificate) -> Vec<u8> {
    let list = SignatureList::X509(SignatureData::new_x509(
        OWNER_1,
        Cow::Owned(signcert.to_der().unwrap()),
    ));
    let mut buf = Vec::new();
    list.extend_as_spec_signature_list(&mut buf);
    buf
}

fn test_pkey() -> RsaKeyPair {
    RsaKeyPair::generate(1024).unwrap()
}

fn test_x509(key: &RsaKeyPair) -> X509Certificate {
    X509Certificate::build_self_signed(
        key,
        "US",
        "Washington",
        "Redmond",
        "Example Organization",
        "example.com",
    )
    .unwrap()
}

fn test_pkcs7_data(input: &[u8]) -> Vec<u8> {
    let pkey = test_pkey();
    let signcert = test_x509(&pkey);
    Pkcs7SignedData::sign(&signcert, &pkey, input)
        .unwrap()
        .to_der()
        .unwrap()
}
