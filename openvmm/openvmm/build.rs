// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn numeric_version() -> [u16; 3] {
    let version = openvmm_build_info::get().product_version();
    let components = version.split('.').collect::<Vec<_>>();
    let [major, minor, patch] = components.as_slice() else {
        panic!("OpenVMM product version must contain exactly three components, got {version:?}");
    };
    let parse = |name: &str, value: &str| {
        value.parse().unwrap_or_else(|_| {
            panic!("OpenVMM product {name} component must be an unsigned 16-bit integer")
        })
    };
    [
        parse("major", major),
        parse("minor", minor),
        parse("patch", patch),
    ]
}

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");

    let [major, minor, patch] = numeric_version();

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=onecore_apiset");
        println!("cargo:rustc-link-lib=onecoreuap_apiset");

        // Embed version/resource info into the EXE.
        println!("cargo:rerun-if-changed=resources.rc");

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
