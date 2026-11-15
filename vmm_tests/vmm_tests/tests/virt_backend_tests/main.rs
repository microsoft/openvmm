// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native hypervisor contract tests without VM boot or guest artifacts.

cfg_select! {
    all(
        guest_arch = "x86_64",
        guest_is_native,
        any(target_os = "linux", windows)
    ) => {
        #[cfg(target_os = "linux")]
        mod activity;
        #[cfg(target_os = "linux")]
        mod apic;
        mod fixture;
        #[cfg(target_os = "linux")]
        mod init_sipi;
        mod msi;
        mod msr_state;
        mod native;
        mod nmi;
        #[cfg(target_os = "linux")]
        mod registers;
        #[cfg(target_os = "linux")]
        mod restore;
        #[cfg(target_os = "linux")]
        mod save;
        mod shadow;
        mod speculation;
        mod time;
        mod unsupported_state;
        #[cfg(target_os = "linux")]
        mod xsave;
    }
    _ => {}
}

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
