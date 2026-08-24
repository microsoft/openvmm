// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setup the environment variables that the VMM tests require to run.

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Serialize, Deserialize)]
pub struct PetriParams {
    /// Disable lazy remote artifact fetching (set PETRI_REMOTE_ARTIFACTS=0).
    /// Should be true in CI where all images are pre-downloaded.
    pub disable_remote_artifacts: bool,
    /// Whether to reuse VHDs created with prep_steps
    pub reuse_prepped_vhds: bool,
    /// Tell petri to expect 2mb hugetlb support
    pub require_2mb_hugetlb: bool,
}

flowey_request! {
    pub struct Request {
        /// Directory to symlink / copy test contents into. Does not need to be
        /// empty.
        pub test_content_dir: ReadVar<PathBuf>,
        /// Specify where VMM tests disk images are stored.
        pub disk_images_dir: Option<ReadVar<PathBuf>>,
        /// What triple VMM tests are built for.
        ///
        /// Used to detect cases of running Windows VMM tests via WSL2, and adjusting
        /// reported paths appropriately.
        pub vmm_tests_target: target_lexicon::Triple,
        /// Get the path to the folder containing various logs emitted VMM tests.
        pub get_test_log_path: Option<WriteVar<PathBuf>>,

        /// Parameters to pass to Petri via environment variables
        pub petri_params: PetriParams,

        /// Get a map of env vars required to be set when running VMM tests
        pub get_env: WriteVar<BTreeMap<String, String>>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_content_dir,
            vmm_tests_target,
            disk_images_dir,
            get_test_log_path,

            petri_params:
                PetriParams {
                    disable_remote_artifacts,
                    reuse_prepped_vhds,
                    require_2mb_hugetlb,
                },

            get_env,
        } = request;

        // In CI, unstable test failures are non-gating and should be reported as
        // passing (with a warning). Outside of CI, unstable test failures are
        // reported as failures unless the user explicitly opts in.
        let ignore_unstable_failures = !matches!(ctx.backend(), FlowBackend::Local);

        ctx.emit_rust_step("setting up vmm_tests env", |ctx| {
            let test_content_dir = test_content_dir.claim(ctx);
            let get_env = get_env.claim(ctx);
            let get_test_log_path = get_test_log_path.claim(ctx);
            let disk_image_dir = disk_images_dir.claim(ctx);
            move |rt| {
                let test_content_dir = rt.read(test_content_dir);

                let test_log_dir = test_content_dir.join("test_results");
                let temp_dir = test_content_dir.join("temp");

                let mut env = BTreeMap::new();

                let windows_via_wsl2 = flowey_lib_common::_util::running_in_wsl(rt)
                    && matches!(
                        vmm_tests_target.operating_system,
                        target_lexicon::OperatingSystem::Windows
                    );

                let disk_image_dir = disk_image_dir.map(|v| rt.read(v));

                // Convert a path via wslpath if running under WSL2,
                // otherwise just make it absolute.
                let wsl_convert_path = |path: &Path| -> anyhow::Result<String> {
                    if windows_via_wsl2 {
                        Ok(flowey_lib_common::_util::wslpath::linux_to_win(rt, path))
                    } else {
                        std::path::absolute(path)
                            .with_context(|| format!("invalid path {}", path.display()))
                    }
                    .map(|p| p.to_string_lossy().into())
                };

                // Eagerly convert all known paths.
                let converted_content_dir = wsl_convert_path(&test_content_dir)?;
                let converted_log_dir = wsl_convert_path(&test_log_dir)?;
                let converted_temp_dir = wsl_convert_path(&temp_dir)?;
                let converted_disk_image_dir = disk_image_dir
                    .as_ref()
                    .map(|p| wsl_convert_path(p))
                    .transpose()?;

                if !test_content_dir.exists() {
                    fs_err::create_dir_all(&test_content_dir)?
                };

                env.insert("VMM_TESTS_CONTENT_DIR".into(), converted_content_dir);

                if test_log_dir.exists() {
                    fs_err::remove_dir_all(&test_log_dir)?;
                };
                fs_err::create_dir(&test_log_dir)?;
                env.insert("TEST_OUTPUT_PATH".into(), converted_log_dir);

                if !temp_dir.exists() {
                    fs_err::create_dir(&temp_dir)?
                };

                if matches!(rt.platform().kind(), FlowPlatformKind::Windows) || windows_via_wsl2 {
                    env.insert("TEMP".into(), converted_temp_dir.clone());
                    env.insert("TMP".into(), converted_temp_dir.clone());
                    env.insert("SystemTemp".into(), converted_temp_dir);
                } else {
                    env.insert("TMPDIR".into(), converted_temp_dir);
                }

                if let Some(disk_image_dir) = converted_disk_image_dir {
                    env.insert("VMM_TEST_IMAGES".into(), disk_image_dir);
                }

                if disable_remote_artifacts {
                    env.insert("PETRI_REMOTE_ARTIFACTS".into(), "0".into());
                }

                if reuse_prepped_vhds {
                    env.insert("PETRI_REUSE_PREPPED_VHDS".into(), "1".into());
                }

                if ignore_unstable_failures {
                    env.insert("PETRI_IGNORE_UNSTABLE_FAILURES".into(), "1".into());
                }

                if require_2mb_hugetlb {
                    env.insert("OPENVMM_REQUIRE_2MB_HUGETLB".into(), "1".into());
                };

                rt.write(get_env, &env);

                if let Some(var) = get_test_log_path {
                    rt.write(var, &test_log_dir)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
