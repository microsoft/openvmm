// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native hypervisor contract tests without VM boot or guest artifacts.

#![forbid(unsafe_code)]

#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod fixture;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod native;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod time;

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
