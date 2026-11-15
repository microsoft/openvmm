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
        mod msr_state;
        mod native;
        #[cfg(target_os = "linux")]
        mod registers;
        #[cfg(target_os = "linux")]
        mod save;
        mod speculation;
        mod time;
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
