// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn parse_component(name: &str, value: &str) -> u16 {
    value
        .parse()
        .unwrap_or_else(|_| panic!("{name} must be an unsigned 16-bit integer, got {value:?}"))
}

fn product_version() -> [u16; 3] {
    let version_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../VERSION");
    let version = std::fs::read_to_string(&version_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", version_path.display()));
    let version = version.trim();
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

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../VERSION");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=onecore_apiset");
        println!("cargo:rustc-link-lib=onecoreuap_apiset");

        // Embed version/resource info into the EXE.
        println!("cargo:rerun-if-changed=resources.rc");

        let [major, minor, patch] = product_version();
        let revision = 0;

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
