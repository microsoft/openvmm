// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=onecore_apiset");
        println!("cargo:rustc-link-lib=onecoreuap_apiset");

        // Embed version/resource info into the EXE.
        println!("cargo:rerun-if-changed=resources.rc");
        let mut version = env!("CARGO_PKG_VERSION").split('.');
        let mut next_component = || {
            version
                .next()
                .and_then(|component| component.parse::<u16>().ok())
                .expect("OpenVMM's Cargo version must be MAJOR.MINOR.PATCH with u16 components")
        };
        let major = next_component();
        let minor = next_component();
        let patch = next_component();
        assert!(
            version.next().is_none(),
            "OpenVMM's Cargo version must contain exactly three components"
        );
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
