// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a nextest archive inside an incubator (e.g., QEMU TCG).
//!
//! This node encapsulates the logic for invoking the `incubator` binary
//! with a nextest archive, used by both the local (`vmm-tests-run`) and
//! CI (`consume_and_test_nextest_vmm_tests_archive`) paths.

use crate::run_cargo_nextest_run::NextestProfile;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Path to the incubator binary.
        pub incubator_bin: ReadVar<PathBuf>,
        /// Path to the incubator profile TOML file.
        pub profile_path: ReadVar<PathBuf>,
        /// Path to the guest kernel image.
        pub kernel: ReadVar<PathBuf>,
        /// Path to the base initrd.
        pub initrd: ReadVar<PathBuf>,
        /// Directory to share into the VM at `/share`.
        /// Must contain the nextest archive and cargo-nextest binary.
        pub share_dir: ReadVar<PathBuf>,
        /// Filename of the nextest archive (relative to share_dir).
        pub nextest_archive_name: ReadVar<String>,
        /// Nextest filter expression.
        pub nextest_filter_expr: Option<String>,
        /// Nextest profile to use.
        pub nextest_profile: NextestProfile,
        /// Additional environment variables.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Path to the QEMU binary (overrides the profile's binary setting).
        pub qemu_binary: Option<ReadVar<PathBuf>>,
        /// Wait for specified side-effects before running.
        pub pre_run_deps: Vec<ReadVar<SideEffect>>,
        /// Results of running the tests.
        pub results: WriteVar<TestResults>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            incubator_bin,
            profile_path,
            kernel,
            initrd,
            share_dir,
            nextest_archive_name,
            nextest_filter_expr,
            nextest_profile,
            extra_env,
            qemu_binary,
            pre_run_deps,
            results,
        } = request;

        ctx.emit_rust_step("run tests in incubator", |ctx| {
            let incubator_bin = incubator_bin.claim(ctx);
            let profile_path = profile_path.claim(ctx);
            let kernel = kernel.claim(ctx);
            let initrd = initrd.claim(ctx);
            let share_dir = share_dir.claim(ctx);
            let nextest_archive_name = nextest_archive_name.claim(ctx);
            let extra_env = extra_env.claim(ctx);
            let qemu_binary = qemu_binary.claim(ctx);
            let results = results.claim(ctx);
            for dep in pre_run_deps {
                dep.claim(ctx);
            }

            move |rt| {
                let incubator_bin = rt.read(incubator_bin);
                let profile_path = rt.read(profile_path);
                let kernel = rt.read(kernel);
                let initrd = rt.read(initrd);
                let share_dir = rt.read(share_dir);
                let archive_name = rt.read(nextest_archive_name);
                let extra_env = extra_env.map(|v| rt.read(v));
                let qemu_binary = qemu_binary.map(|v| rt.read(v));

                let nextest_bin_name = "cargo-nextest";
                let guest_nextest = format!("/share/{nextest_bin_name}");
                let guest_archive = format!("/share/{archive_name}");

                log::info!(
                    "Launching incubator with profile: {}",
                    profile_path.display()
                );

                let mut cmd = std::process::Command::new(&incubator_bin);
                cmd.arg("--profile")
                    .arg(&profile_path)
                    .arg("--kernel")
                    .arg(&kernel)
                    .arg("--initrd")
                    .arg(&initrd)
                    .arg("--share")
                    .arg(&share_dir);

                if let Some(ref qemu_binary) = qemu_binary {
                    cmd.arg("--qemu-binary").arg(qemu_binary);
                }

                cmd.arg("--")
                    .arg(&guest_nextest)
                    .arg("nextest")
                    .arg("run")
                    .arg("--archive-file")
                    .arg(&guest_archive)
                    .arg("--workspace-remap")
                    .arg("/share");

                if let Some(ref filter) = nextest_filter_expr {
                    cmd.arg("--filter-expr").arg(filter);
                }

                let profile_str = nextest_profile.as_str();
                cmd.arg("-P").arg(profile_str);

                if let Some(ref env) = extra_env {
                    for (k, v) in env {
                        cmd.env(k, v);
                    }
                }

                let status = cmd.status().context("failed to launch incubator")?;

                let all_tests_passed = status.success();
                let junit_xml = {
                    let junit_path = share_dir.join("target/nextest/ci/junit.xml");
                    if junit_path.exists() {
                        Some(junit_path)
                    } else {
                        None
                    }
                };

                rt.write(
                    results,
                    &TestResults {
                        all_tests_passed,
                        junit_xml,
                    },
                );

                Ok(())
            }
        });

        Ok(())
    }
}
