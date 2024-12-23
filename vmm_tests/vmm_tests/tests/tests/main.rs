// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A collection of end-to-end VMM tests.
//!
//! Tests should contain both the name of the firmware and the guest they are
//! using, so that our test runners can easily filter them.
//!
//! If you use the #[vmm_test] macro then all of the above requirements
//! are handled for you automatically.

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
// Tests that will only ever compile and run when targeting x86-64.
#[cfg(guest_arch = "x86_64")]
mod x86_64_exclusive;

/// Common prelude shared by all VMM tests.
mod prelude {
    /// Obtain a new  [`petri::TestArtifactResolver`]
    // DEVNOTE: this method is referenced by the `vmm_test` macro
    // in order to let consuming crates easily configure what artifact resolver
    // is being used.
    //
    // In order to change the name / signature of this method, you must also
    // update the macro code!
    pub fn vmm_tests_artifact_resolver() -> petri::TestArtifactResolver {
        if std::env::var("VMM_TEST_LIST_TEST_DEPS").is_ok() {
            petri::TestArtifactResolver::new(Box::new(
                vmm_test_petri_support::list_test_deps_resolver::ListTestDepsArtifactResolver::default(),
            ))
        } else {
            petri::TestArtifactResolver::new(Box::new(
                petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver,
            ))
        }
    }
}
