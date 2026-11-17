// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native hypervisor contract tests without VM boot or guest artifacts.

#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod activity;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod apic;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod fixture;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod init_sipi;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod msi;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod msr_state;
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
mod nmi;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod registers;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod restore;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod save;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod shadow;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod smm;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod speculation;
#[cfg(all(guest_arch = "x86_64", guest_is_native, windows))]
mod suspend;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod time;
#[cfg(all(
    guest_arch = "x86_64",
    guest_is_native,
    any(target_os = "linux", windows)
))]
mod unsupported_state;
#[cfg(all(guest_arch = "x86_64", guest_is_native, target_os = "linux"))]
mod xsave;

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
