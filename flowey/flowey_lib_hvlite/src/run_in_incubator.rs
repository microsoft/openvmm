// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run a nextest archive inside an incubator (e.g., QEMU TCG).
//!
//! This node encapsulates the logic for invoking the `incubator` binary
//! with a nextest archive, used by both the local (`vmm-tests-run`) and
//! CI (`consume_and_test_nextest_vmm_tests_archive`) paths.

use crate::run_cargo_nextest_run::NextestProfile;
use anyhow::Context;
use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::TestResults;
use std::collections::BTreeMap;
use std::path::Path;

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
        /// Path to the OpenVMM repo root. Used as nextest's workspace remap.
        pub workspace_dir: ReadVar<PathBuf>,
        /// Directory containing VMM test runtime artifacts and test outputs.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Host path to the nextest archive.
        pub nextest_archive: ReadVar<PathBuf>,
        /// Host path to the guest cargo-nextest binary.
        pub nextest_bin: ReadVar<PathBuf>,
        /// Path to the nextest config file (e.g. the repo's
        /// `.config/nextest.toml`).
        pub nextest_config_file: ReadVar<PathBuf>,
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
            workspace_dir,
            test_content_dir,
            nextest_archive,
            nextest_bin,
            nextest_config_file,
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
            let workspace_dir = workspace_dir.claim(ctx);
            let test_content_dir = test_content_dir.claim(ctx);
            let nextest_archive = nextest_archive.claim(ctx);
            let nextest_bin = nextest_bin.claim(ctx);
            let nextest_config_file = nextest_config_file.claim(ctx);
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
                let workspace_dir = rt.read(workspace_dir).absolute()?;
                let test_content_dir = rt.read(test_content_dir).absolute()?;
                let nextest_archive = rt.read(nextest_archive).absolute()?;
                let nextest_bin = rt.read(nextest_bin).absolute()?;
                let nextest_config_file = rt.read(nextest_config_file).absolute()?;
                let mut extra_env = extra_env.map(|v| rt.read(v)).unwrap_or_default();
                let qemu_binary = qemu_binary.map(|v| rt.read(v));

                let mut share_paths = vec![
                    workspace_dir.as_path(),
                    test_content_dir.as_path(),
                    nextest_archive.as_path(),
                    nextest_bin.as_path(),
                    nextest_config_file.as_path(),
                ];
                let images_dir = extra_env.get("VMM_TEST_IMAGES").map(PathBuf::from);
                if let Some(ref images_dir) = images_dir {
                    share_paths.push(images_dir.as_path());
                }
                let share_root = common_ancestor(&share_paths)?;

                let guest_path = |path: &Path| -> anyhow::Result<String> {
                    let relative = path.strip_prefix(&share_root).with_context(|| {
                        format!(
                            "{} is not under share root {}",
                            path.display(),
                            share_root.display()
                        )
                    })?;

                    if relative.as_os_str().is_empty() {
                        Ok("/share".to_string())
                    } else {
                        Ok(format!("/share/{}", relative.display()))
                    }
                };

                let guest_nextest = guest_path(&nextest_bin)?;
                let guest_archive = guest_path(&nextest_archive)?;
                let guest_workspace = guest_path(&workspace_dir)?;
                let guest_config = guest_path(&nextest_config_file)?;
                let guest_test_content_dir = guest_path(&test_content_dir)?;
                let guest_output_dir = format!("{guest_test_content_dir}/test_results");

                extra_env.insert(
                    "VMM_TESTS_CONTENT_DIR".into(),
                    guest_test_content_dir.clone(),
                );
                extra_env.insert("TEST_OUTPUT_PATH".into(), guest_output_dir.clone());
                if let Some(images) = extra_env.get_mut("VMM_TEST_IMAGES") {
                    *images = guest_path(Path::new(images))?;
                }

                log::info!(
                    "Launching incubator with profile: {}",
                    profile_path.display()
                );

                // Artifact upload/download strips the executable bit, so
                // restore it before launching the incubator binary.
                incubator_bin.make_executable()?;
                nextest_bin.make_executable()?;

                let mut cmd = std::process::Command::new(&incubator_bin);
                cmd.arg("--profile")
                    .arg(&profile_path)
                    .arg("--kernel")
                    .arg(&kernel)
                    .arg("--initrd")
                    .arg(&initrd)
                    .arg("--share")
                    .arg(&share_root)
                    .arg("--output-dir")
                    .arg(test_content_dir.join("test_results"))
                    .arg("--guest-pipette")
                    .arg(format!("{guest_test_content_dir}/pipette"))
                    .arg("--guest-current-dir")
                    .arg(&guest_test_content_dir);

                for (k, v) in &extra_env {
                    cmd.arg("--guest-env").arg(format!("{k}={v}"));
                }

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
                    .arg(&guest_workspace)
                    .arg("--config-file")
                    .arg(&guest_config);

                if let Some(ref filter) = nextest_filter_expr {
                    cmd.arg("--filter-expr").arg(filter);
                }

                let profile_str = nextest_profile.as_str();
                cmd.arg("-P").arg(profile_str);

                let status = cmd.status().context("failed to launch incubator")?;

                let all_tests_passed = status.success();
                let junit_xml = if let Some(junit_path) =
                    flowey_lib_common::run_cargo_nextest_run::nextest_junit_path(
                        &nextest_config_file,
                        profile_str,
                    )? {
                    let junit_path = test_content_dir
                        .join("target")
                        .join("nextest")
                        .join(profile_str)
                        .join(junit_path);
                    junit_path.exists().then_some(junit_path)
                } else {
                    None
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

fn common_ancestor(paths: &[&Path]) -> anyhow::Result<PathBuf> {
    let mut candidate = paths
        .first()
        .context("no paths for share root")?
        .to_path_buf();

    loop {
        if paths.iter().all(|path| path.starts_with(&candidate)) {
            return Ok(candidate);
        }

        if !candidate.pop() {
            anyhow::bail!("paths do not share a common root")
        }
    }
}
