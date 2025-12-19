// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test entrypoint for running TMK tests in different environments.

#![forbid(unsafe_code)]

// Include all the tests.
//
// FUTURE: probably the tmk_tests package should own this crate, rather than
// reuse vmm_tests. But this creates a bunch of work for CI. Revisit this once
// CI is a little less cumbersome to modify.
use tmk_tests as _;

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
