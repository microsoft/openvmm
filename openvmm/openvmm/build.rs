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

        // Default to the crate version so that the version Windows reports in
        // the file properties and the one `openvmm --version` prints cannot
        // disagree. The `OPENVMM_*` vars still win, which is how a build
        // pipeline stamps its own build number in. There is no crate
        // equivalent of the fourth component, so it stays 0 unless set.
        let parse_u16 = |s: String| s.parse::<u16>().unwrap_or(0);
        let component = |var: &str, from_crate_version: &str| {
            std::env::var(var)
                .or_else(|_| std::env::var(from_crate_version))
                .map(parse_u16)
                .unwrap_or(0)
        };
        let major = component("OPENVMM_MAJOR", "CARGO_PKG_VERSION_MAJOR");
        let minor = component("OPENVMM_MINOR", "CARGO_PKG_VERSION_MINOR");
        let patch = component("OPENVMM_PATCH", "CARGO_PKG_VERSION_PATCH");
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
