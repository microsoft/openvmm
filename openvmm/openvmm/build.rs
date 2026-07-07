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
        println!("cargo:rerun-if-env-changed=OPENVMM_MAJOR");
        println!("cargo:rerun-if-env-changed=OPENVMM_MINOR");
        println!("cargo:rerun-if-env-changed=OPENVMM_PATCH");
        println!("cargo:rerun-if-env-changed=OPENVMM_REVISION");

        let parse_u16 = |s: String| s.parse::<u16>().unwrap_or(0);
        let major = std::env::var("OPENVMM_MAJOR").map(parse_u16).unwrap_or(0);
        let minor = std::env::var("OPENVMM_MINOR").map(parse_u16).unwrap_or(0);
        let patch = std::env::var("OPENVMM_PATCH").map(parse_u16).unwrap_or(0);
        let revision = std::env::var("OPENVMM_REVISION")
            .map(parse_u16)
            .unwrap_or(0);

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
