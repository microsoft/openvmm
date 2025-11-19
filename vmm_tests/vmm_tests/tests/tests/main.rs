// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![forbid(unsafe_code)]

//! A collection of end-to-end VMM tests.
//!
//! Tests should contain both the name of the firmware and the guest they are
//! using, so that our test runners can easily filter them.
//!
//! If you use the #[vmm_test] macro then all of the above requirements
//! are handled for you automatically.
//!
//! Not all tests are expected to work in all scenarios. For example, Hyper-V
//! tests do not work in WSL and TDX tests require a TDX-capable CPU.

// Tests that run on more than one architecture.
mod multiarch;
// Tests for the TTRPC interface that currently only run on x86-64 but can
// compile when targeting any architecture. As our ARM64 support improves
// these tests should be able to someday run on both x86-64 and ARM64, and be
// moved into a multi-arch module.
mod ttrpc;
// Tests that currently run only on x86-64 but can compile when targeting
// any architecture. As our ARM64 support improves these tests should be able to
// someday run on both x86-64 and ARM64, and be moved into a multi-arch module.
mod x86_64;
// Tests that will only ever run when targeting x86-64.
mod x86_64_exclusive;
// Tests that will only ever run targeting Aarch64/ARM64.
mod aarch64_exclusive;

pub fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
