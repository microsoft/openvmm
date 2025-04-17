// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test entrypoint for running TMK tests in different environments.

// Include all the tests.
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
