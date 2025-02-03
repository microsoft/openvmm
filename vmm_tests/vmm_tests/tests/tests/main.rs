// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A collection of end-to-end VMM tests.
//!
//! Tests should contain both the name of the firmware and the guest they are
//! using, so that our test runners can easily filter them.
//!
//! If you use the #[vmm_test] macro then all of the above requirements
//! are handled for you automatically.

mod test;

use test::multitest;
use test::test;
use test::SimpleTest;

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

#[derive(clap::Parser)]
struct Options {
    /// Lists the required artifacts for all tests.
    #[clap(long)]
    list_required_artifacts: bool,
    #[clap(flatten)]
    inner: libtest_mimic::Arguments,
}

pub fn main() {
    let mut args = <Options as clap::Parser>::parse();
    if args.list_required_artifacts {
        // FUTURE: write this in a machine readable format.
        for test in test::Test::all() {
            let requirements = test.requirements();
            println!("{}:", test.name());
            for artifact in requirements.required_artifacts() {
                println!("required: {artifact:?}");
            }
            for artifact in requirements.optional_artifacts() {
                println!("optional: {artifact:?}");
            }
            println!();
        }
        return;
    }

    // Always just use one thread to avoid interleaving logs and to avoid using
    // too many resources. These tests are usually run under nextest, which will
    // run them in parallel in separate processes with appropriate concurrency
    // limits.
    args.inner.test_threads = Some(1);

    let trials = test::Test::all()
        .map(|test| {
            test.trial(|name, requirements| {
                requirements.resolve(
                petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(name))
            })
        })
        .collect();

    libtest_mimic::run(&args.inner, trials).exit()
}
