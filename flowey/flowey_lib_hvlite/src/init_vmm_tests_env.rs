// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setup the environment variables and directory structure that the VMM tests
//! require to run.

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::download_openvmm_deps::OpenvmmDepsArch;
use crate::download_release_igvm_files::Release2411Output;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

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

        /// Register an openvmm binary
        pub register_openvmm: Option<ReadVar<crate::build_openvmm::OpenvmmOutput>>,
        /// Register a windows pipette binary
        pub register_pipette_windows: Option<ReadVar<crate::build_pipette::PipetteOutput>>,
        /// Register a linux-musl pipette binary
        pub register_pipette_linux_musl: Option<ReadVar<crate::build_pipette::PipetteOutput>>,
        /// Register a guest_test_uefi image
        pub register_guest_test_uefi:
            Option<ReadVar<crate::build_guest_test_uefi::GuestTestUefiOutput>>,
        /// Register OpenHCL IGVM files
        pub register_openhcl_igvm_files: Option<
            ReadVar<
                Vec<(
                    OpenhclIgvmRecipe,
                    crate::run_igvmfilegen::IgvmOutput,
                )>,
            >,
        >,

        /// Get the path to the folder containing various logs emitted VMM tests.
        pub get_test_log_path: Option<WriteVar<PathBuf>>,
        /// Get a map of env vars required to be set when running VMM tests
        pub get_env: WriteVar<BTreeMap<String, String>>,
        pub release_2411_igvm_files: ReadVar<Release2411Output>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_openvmm_deps::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_content_dir,
            vmm_tests_target,
            register_openvmm,
            register_pipette_windows,
            register_pipette_linux_musl,
            register_guest_test_uefi,
            disk_images_dir,
            register_openhcl_igvm_files,
            get_test_log_path,
            get_env,
            release_2411_igvm_files,
        } = request;

        let openvmm_deps_arch = match vmm_tests_target.architecture {
            target_lexicon::Architecture::X86_64 => OpenvmmDepsArch::X86_64,
            target_lexicon::Architecture::Aarch64(_) => OpenvmmDepsArch::Aarch64,
            arch => anyhow::bail!("unsupported arch {arch}"),
        };

        let test_linux_initrd = ctx.reqv(|v| {
            crate::download_openvmm_deps::Request::GetLinuxTestInitrd(openvmm_deps_arch, v)
        });
        let test_linux_kernel = ctx.reqv(|v| {
            crate::download_openvmm_deps::Request::GetLinuxTestKernel(openvmm_deps_arch, v)
        });

        let openvmm_repo_root = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("setting up vmm_tests env", |ctx| {
            let test_content_dir = test_content_dir.claim(ctx);
            let get_env = get_env.claim(ctx);
            let get_test_log_path = get_test_log_path.claim(ctx);
            let openvmm = register_openvmm.claim(ctx);
            let pipette_win = register_pipette_windows.claim(ctx);
            let pipette_linux = register_pipette_linux_musl.claim(ctx);
            let guest_test_uefi = register_guest_test_uefi.claim(ctx);
            let disk_image_dir = disk_images_dir.claim(ctx);
            let openhcl_igvm_files = register_openhcl_igvm_files.claim(ctx);
            let test_linux_initrd = test_linux_initrd.claim(ctx);
            let test_linux_kernel = test_linux_kernel.claim(ctx);
            let openvmm_repo_root = openvmm_repo_root.claim(ctx);
            let release_2411_igvm_files = release_2411_igvm_files.claim(ctx);
            move |rt| {
                let test_linux_initrd = rt.read(test_linux_initrd);
                let test_linux_kernel = rt.read(test_linux_kernel);

                let test_content_dir = rt.read(test_content_dir);
                let openvmm_repo_root = rt.read(openvmm_repo_root);

                let mut env = BTreeMap::new();

                let windows_openvmm_via_wsl2 = flowey_lib_common::_util::running_in_wsl(rt)
                    && matches!(
                        vmm_tests_target.operating_system,
                        target_lexicon::OperatingSystem::Windows
                    );

                let path_as_string = |path: &Path| -> anyhow::Result<String> {
                    Ok(if windows_openvmm_via_wsl2 {
                        flowey_lib_common::_util::wslpath::linux_to_win(path)
                            .display()
                            .to_string()
                    } else {
                        path.absolute()
                            .context(format!("invalid path {}", path.display()))?
                            .display()
                            .to_string()
                    })
                };

                env.insert(
                    "VMM_TESTS_CONTENT_DIR".into(),
                    path_as_string(&test_content_dir)?,
                );

                env.insert(
                    "VMM_TESTS_REPO_ROOT".into(),
                    path_as_string(&openvmm_repo_root)?,
                );

                // use a subdir for test logs
                let test_log_dir = test_content_dir.join("test_results");
                if !test_log_dir.exists() {
                    fs_err::create_dir(&test_log_dir)?
                };
                env.insert("TEST_OUTPUT_PATH".into(), path_as_string(&test_log_dir)?);

                if let Some(disk_image_dir) = disk_image_dir {
                    env.insert(
                        "VMM_TEST_IMAGES".into(),
                        path_as_string(&rt.read(disk_image_dir))?,
                    );
                }

                if let Some(openvmm) = openvmm {
                    // TODO OSS: update filenames to use openvmm naming (requires petri updates)
                    match rt.read(openvmm) {
                        crate::build_openvmm::OpenvmmOutput::WindowsBin { exe, pdb: _ } => {
                            fs_err::copy(exe, test_content_dir.join("openvmm.exe"))?;
                        }
                        crate::build_openvmm::OpenvmmOutput::LinuxBin { bin, dbg: _ } => {
                            let dst = test_content_dir.join("openvmm");
                            fs_err::copy(bin, dst.clone())?;

                            // make sure openvmm is executable
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let old_mode = dst.metadata()?.permissions().mode();
                                fs_err::set_permissions(
                                    dst,
                                    std::fs::Permissions::from_mode(old_mode | 0o111),
                                )?;
                            }
                        }
                    }
                }

                if let Some(pipette_win) = pipette_win {
                    match rt.read(pipette_win) {
                        crate::build_pipette::PipetteOutput::WindowsBin { exe, pdb: _ } => {
                            fs_err::copy(exe, test_content_dir.join("pipette.exe"))?;
                        }
                        _ => anyhow::bail!("did not find `pipette.exe` in RegisterPipetteWindows"),
                    }
                }

                if let Some(pipette_linux) = pipette_linux {
                    match rt.read(pipette_linux) {
                        crate::build_pipette::PipetteOutput::LinuxBin { bin, dbg: _ } => {
                            fs_err::copy(bin, test_content_dir.join("pipette"))?;
                        }
                        _ => {
                            anyhow::bail!("did not find `pipette.exe` in RegisterPipetteLinuxMusl")
                        }
                    }
                }

                if let Some(guest_test_uefi) = guest_test_uefi {
                    let crate::build_guest_test_uefi::GuestTestUefiOutput {
                        efi: _,
                        pdb: _,
                        img,
                    } = rt.read(guest_test_uefi);
                    fs_err::copy(img, test_content_dir.join("guest_test_uefi.img"))?;
                }

                if let Some(openhcl_igvm_files) = openhcl_igvm_files {
                    for (recipe, openhcl_igvm) in rt.read(openhcl_igvm_files) {
                        let crate::run_igvmfilegen::IgvmOutput { igvm_bin, .. } = openhcl_igvm;

                        let filename = match recipe {
                            OpenhclIgvmRecipe::X64 => "openhcl-x64.bin",
                            OpenhclIgvmRecipe::X64Cvm => "openhcl-x64-cvm.bin",
                            OpenhclIgvmRecipe::X64TestLinuxDirect => {
                                "openhcl-x64-test-linux-direct.bin"
                            }
                            OpenhclIgvmRecipe::Aarch64 => "openhcl-aarch64.bin",
                            _ => {
                                log::info!("petri doesn't support this OpenHCL recipe: {recipe:?}");
                                continue;
                            }
                        };

                        fs_err::copy(igvm_bin, test_content_dir.join(filename))?;
                    }
                }

                let release_2411_igvm_files = rt.read(release_2411_igvm_files);
                fs_err::copy(
                    release_2411_igvm_files.x64_bin,
                    test_content_dir.join("release-2411-x64-openhcl.bin"),
                )?;
                fs_err::copy(
                    release_2411_igvm_files.x64_direct_bin,
                    test_content_dir.join("release-2411-x64-openhcl-direct.bin"),
                )?;
                fs_err::copy(
                    release_2411_igvm_files.aarch64_bin,
                    test_content_dir.join("release-2411-aarch64-openhcl.bin"),
                )?;

                let (arch_dir, kernel_file_name) = match openvmm_deps_arch {
                    OpenvmmDepsArch::X86_64 => ("x64", "vmlinux"),
                    OpenvmmDepsArch::Aarch64 => ("aarch64", "Image"),
                };
                fs_err::create_dir_all(test_content_dir.join(arch_dir))?;
                fs_err::copy(
                    test_linux_initrd,
                    test_content_dir.join(arch_dir).join("initrd"),
                )?;
                fs_err::copy(
                    test_linux_kernel,
                    test_content_dir.join(arch_dir).join(kernel_file_name),
                )?;

                // debug log the current contents of the dir
                log::debug!("final folder content: {}", test_content_dir.display());
                for entry in test_content_dir.read_dir()? {
                    let entry = entry?;
                    log::debug!("contains: {:?}", entry.file_name());
                }

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
