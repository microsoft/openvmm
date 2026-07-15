// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn parse_component(name: &str, value: &str) -> u16 {
    value
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be an unsigned 16-bit integer, got {value:?}"))
}

fn product_version() -> [u16; 3] {
    let version = include_str!("../VERSION").trim();
    let components = version.split('.').collect::<Vec<_>>();
    let [major, minor, patch] = components.as_slice() else {
        panic!("OpenVMM VERSION must contain exactly three components, got {version:?}");
    };

    [
        parse_component("OpenVMM VERSION major component", major),
        parse_component("OpenVMM VERSION minor component", minor),
        parse_component("OpenVMM VERSION patch component", patch),
    ]
}

fn resource_version(product_version: [u16; 3]) -> [u16; 4] {
    fn env_component(name: &str, default: u16) -> u16 {
        match std::env::var(name) {
            Ok(value) => parse_component(name, &value),
            Err(std::env::VarError::NotPresent) => default,
            Err(std::env::VarError::NotUnicode(_)) => {
                panic!("{name} must contain valid Unicode");
            }
        }
    }

    let [major, minor, patch] = product_version;
    [
        env_component("OPENVMM_MAJOR", major),
        env_component("OPENVMM_MINOR", minor),
        env_component("OPENVMM_PATCH", patch),
        env_component("OPENVMM_REVISION", 0),
    ]
}

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../VERSION");

    let product_version = product_version();

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=onecore_apiset");
        println!("cargo:rustc-link-lib=onecoreuap_apiset");

        // Embed version/resource info into the EXE.
        println!("cargo:rerun-if-changed=resources.rc");
        println!("cargo:rerun-if-env-changed=OPENVMM_MAJOR");
        println!("cargo:rerun-if-env-changed=OPENVMM_MINOR");
        println!("cargo:rerun-if-env-changed=OPENVMM_PATCH");
        println!("cargo:rerun-if-env-changed=OPENVMM_REVISION");

        let [major, minor, patch, revision] = resource_version(product_version);

        let macros = [
            (
                "OPENVMM_VERSION",
                format!("{major},{minor},{patch},{revision}"),
            ),
            (
                "OPENVMM_VERSION_STR",
                format!(r#""{major}.{minor}.{patch}.{revision}""#),
            ),
        ];

        embed_resource::compile(
            "resources.rc",
            macros.iter().map(|(k, v)| format!("{k}={v}")),
        )
        .manifest_required()
        .expect("Failed to embed resources");
    }
}
