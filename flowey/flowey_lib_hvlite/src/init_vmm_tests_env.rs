// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Setup the environment variables and directory structure that the VMM tests
//! require to run.

use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::download_openvmm_deps::OpenvmmDepsArch;
use crate::download_release_igvm_files_from_gh::OpenhclReleaseVersion;
use crate::download_uefi_mu_msvm::MuMsvmArch;
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
        /// Register TMK VMM binaries.
        pub register_tmks: Option<ReadVar<crate::build_tmks::TmksOutput>>,
        /// Register a TMK VMM native binary
        pub register_tmk_vmm: Option<ReadVar<crate::build_tmk_vmm::TmkVmmOutput>>,
        /// Register a TMK VMM Linux musl binary
        pub register_tmk_vmm_linux_musl: Option<ReadVar<crate::build_tmk_vmm::TmkVmmOutput>>,
        /// Register a vmgstool binary
        pub register_vmgstool: Option<ReadVar<crate::build_vmgstool::VmgstoolOutput>>,
        /// Register a Windows tpm_guest_tests binary
        pub register_tpm_guest_tests_windows: Option<ReadVar<TpmGuestTestsOutput>>,
        /// Register a Linux tpm_guest_tests binary
        pub register_tpm_guest_tests_linux: Option<ReadVar<TpmGuestTestsOutput>>,

        /// Get the path to the folder containing various logs emitted VMM tests.
        pub get_test_log_path: Option<WriteVar<PathBuf>>,
        /// Get a map of env vars required to be set when running VMM tests
        pub get_env: WriteVar<BTreeMap<String, String>>,
        pub release_igvm_files: Option<ReadVar<crate::download_release_igvm_files_from_gh::ReleaseOutput>>,
        /// Use paths relative to `test_content_dir` for environment variables
        pub use_relative_paths: bool,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_openvmm_deps::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            test_content_dir,
            vmm_tests_target,
            register_openvmm,
            register_pipette_windows,
            register_pipette_linux_musl,
            register_guest_test_uefi,
            register_tmks,
            register_tmk_vmm,
            register_tmk_vmm_linux_musl,
            register_vmgstool,
            register_tpm_guest_tests_windows,
            register_tpm_guest_tests_linux,
            disk_images_dir,
            register_openhcl_igvm_files,
            get_test_log_path,
            get_env,
            release_igvm_files,
            use_relative_paths,
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

        let mu_msvm_arch = match vmm_tests_target.architecture {
            target_lexicon::Architecture::X86_64 => MuMsvmArch::X86_64,
            target_lexicon::Architecture::Aarch64(_) => MuMsvmArch::Aarch64,
            arch => anyhow::bail!("unsupported arch {arch}"),
        };
        let uefi = ctx.reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd {
            arch: mu_msvm_arch,
            msvm_fd: v,
        });

        ctx.emit_rust_step("setting up vmm_tests env", |ctx| {
            let test_content_dir = test_content_dir.claim(ctx);
            let get_env = get_env.claim(ctx);
            let get_test_log_path = get_test_log_path.claim(ctx);
            let openvmm = register_openvmm.claim(ctx);
            let pipette_win = register_pipette_windows.claim(ctx);
            let pipette_linux = register_pipette_linux_musl.claim(ctx);
            let guest_test_uefi = register_guest_test_uefi.claim(ctx);
            let tmks = register_tmks.claim(ctx);
            let tmk_vmm = register_tmk_vmm.claim(ctx);
            let tmk_vmm_linux_musl = register_tmk_vmm_linux_musl.claim(ctx);
            let vmgstool = register_vmgstool.claim(ctx);
            let tpm_guest_tests_windows = register_tpm_guest_tests_windows.claim(ctx);
            let tpm_guest_tests_linux = register_tpm_guest_tests_linux.claim(ctx);
            let disk_image_dir = disk_images_dir.claim(ctx);
            let openhcl_igvm_files = register_openhcl_igvm_files.claim(ctx);
            let test_linux_initrd = test_linux_initrd.claim(ctx);
            let test_linux_kernel = test_linux_kernel.claim(ctx);
            let uefi = uefi.claim(ctx);
            let release_igvm_files_dir = release_igvm_files.claim(ctx);
            move |rt| {
                let test_linux_initrd = rt.read(test_linux_initrd);
                let test_linux_kernel = rt.read(test_linux_kernel);
                let uefi = rt.read(uefi);
                let release_igvm_files_dir = rt.read(release_igvm_files_dir);
                let test_content_dir = rt.read(test_content_dir);

                let mut env = BTreeMap::new();

                let windows_via_wsl2 = flowey_lib_common::_util::running_in_wsl(rt)
                    && matches!(
                        vmm_tests_target.operating_system,
                        target_lexicon::OperatingSystem::Windows
                    );

                let working_dir_ref = test_content_dir.as_path();
                let working_dir_win = windows_via_wsl2.then(|| {
                    flowey_lib_common::_util::wslpath::linux_to_win(working_dir_ref)
                        .display()
                        .to_string()
                });
                let maybe_convert_path = |path: &Path| -> anyhow::Result<String> {
                    let path = if windows_via_wsl2 {
                        flowey_lib_common::_util::wslpath::linux_to_win(path)
                    } else {
                        path.absolute()
                            .with_context(|| format!("invalid path {}", path.display()))?
                    };
                    let path = if use_relative_paths {
                        if windows_via_wsl2 {
                            let working_dir_trimmed =
                                working_dir_win.as_ref().unwrap().trim_end_matches('\\');
                            let path_win = path.display().to_string();
                            let path_trimmed = path_win.trim_end_matches('\\');
                            PathBuf::from(format!(
                                "$PSScriptRoot{}",
                                path_trimmed
                                    .strip_prefix(working_dir_trimmed)
                                    .with_context(|| format!(
                                        "{} not in {}",
                                        path_win, working_dir_trimmed
                                    ),)?
                            ))
                        } else {
                            path.strip_prefix(working_dir_ref)
                                .with_context(|| {
                                    format!(
                                        "{} not in {}",
                                        path.display(),
                                        working_dir_ref.display()
                                    )
                                })?
                                .to_path_buf()
                        }
                    } else {
                        path
                    };
                    Ok(path.display().to_string())
                };

                env.insert(
                    "VMM_TESTS_CONTENT_DIR".into(),
                    maybe_convert_path(&test_content_dir)?,
                );

                // use a subdir for test logs
                let test_log_dir = test_content_dir.join("test_results");
                if !test_log_dir.exists() {
                    fs_err::create_dir(&test_log_dir)?
                };
                env.insert(
                    "TEST_OUTPUT_PATH".into(),
                    maybe_convert_path(&test_log_dir)?,
                );

                if let Some(disk_image_dir) = disk_image_dir {
                    env.insert(
                        "VMM_TEST_IMAGES".into(),
                        maybe_convert_path(&rt.read(disk_image_dir))?,
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
                            dst.make_executable()?;
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

                if let Some(tmks) = tmks {
                    let crate::build_tmks::TmksOutput { bin, dbg: _ } = rt.read(tmks);
                    fs_err::copy(bin, test_content_dir.join("simple_tmk"))?;
                }

                if let Some(tmk_vmm) = tmk_vmm {
                    match rt.read(tmk_vmm) {
                        crate::build_tmk_vmm::TmkVmmOutput::WindowsBin { exe, .. } => {
                            fs_err::copy(exe, test_content_dir.join("tmk_vmm.exe"))?;
                        }
                        crate::build_tmk_vmm::TmkVmmOutput::LinuxBin { bin, .. } => {
                            let dst = test_content_dir.join("tmk_vmm");
                            fs_err::copy(bin, &dst)?;
                            dst.make_executable()?;
                        }
                    }
                }

                if let Some(tmk_vmm_linux_musl) = tmk_vmm_linux_musl {
                    let crate::build_tmk_vmm::TmkVmmOutput::LinuxBin { bin, dbg: _ } =
                        rt.read(tmk_vmm_linux_musl)
                    else {
                        anyhow::bail!("invalid tmk_vmm output")
                    };
                    // Note that this overwrites the previous tmk_vmm. That's
                    // OK, they should be the same. Fix this when the resolver
                    // can handle multiple different outputs with the same name.
                    fs_err::copy(bin, test_content_dir.join("tmk_vmm"))?;
                }

                if let Some(vmgstool) = vmgstool {
                    match rt.read(vmgstool) {
                        crate::build_vmgstool::VmgstoolOutput::WindowsBin { exe, .. } => {
                            fs_err::copy(exe, test_content_dir.join("vmgstool.exe"))?;
                        }
                        crate::build_vmgstool::VmgstoolOutput::LinuxBin { bin, .. } => {
                            let dst = test_content_dir.join("vmgstool");
                            fs_err::copy(bin, &dst)?;
                            dst.make_executable()?;
                        }
                    }
                }

                if let Some(tpm_guest_tests_windows) = tpm_guest_tests_windows {
                    let TpmGuestTestsOutput::WindowsBin { exe, .. } =
                        rt.read(tpm_guest_tests_windows)
                    else {
                        anyhow::bail!("expected Windows tpm_guest_tests artifact")
                    };
                    fs_err::copy(exe, test_content_dir.join("tpm_guest_tests.exe"))?;
                }

                if let Some(tpm_guest_tests_linux) = tpm_guest_tests_linux {
                    let TpmGuestTestsOutput::LinuxBin { bin, .. } = rt.read(tpm_guest_tests_linux)
                    else {
                        anyhow::bail!("expected Linux tpm_guest_tests artifact")
                    };
                    let dst = test_content_dir.join("tpm_guest_tests");
                    fs_err::copy(bin, &dst)?;
                    dst.make_executable()?;
                }

                if let Some(openhcl_igvm_files) = openhcl_igvm_files {
                    for (recipe, openhcl_igvm) in rt.read(openhcl_igvm_files) {
                        let crate::run_igvmfilegen::IgvmOutput { igvm_bin, .. } = openhcl_igvm;

                        let filename = match recipe {
                            OpenhclIgvmRecipe::X64 => "openhcl-x64.bin",
                            OpenhclIgvmRecipe::X64Devkern => "openhcl-x64-devkern.bin",
                            OpenhclIgvmRecipe::X64Cvm => "openhcl-x64-cvm.bin",
                            OpenhclIgvmRecipe::X64TestLinuxDirect => {
                                "openhcl-x64-test-linux-direct.bin"
                            }
                            OpenhclIgvmRecipe::Aarch64 => "openhcl-aarch64.bin",
                            OpenhclIgvmRecipe::Aarch64Devkern => "openhcl-aarch64-devkern.bin",
                            _ => {
                                log::info!("petri doesn't support this OpenHCL recipe: {recipe:?}");
                                continue;
                            }
                        };

                        fs_err::copy(igvm_bin, test_content_dir.join(filename))?;
                    }
                }

                if let Some(release_igvm_files) = release_igvm_files_dir {
                    let latest_release_version = OpenhclReleaseVersion::latest();

                    if let Some(src) = &release_igvm_files.openhcl {
                        let new_name = format!("{latest_release_version}-x64-openhcl.bin");
                        fs_err::copy(src, test_content_dir.join(new_name))?;
                    }

                    if let Some(src) = &release_igvm_files.openhcl_aarch64 {
                        let new_name = format!("{latest_release_version}-aarch64-openhcl.bin");
                        fs_err::copy(src, test_content_dir.join(new_name))?;
                    }

                    if let Some(src) = &release_igvm_files.openhcl_direct {
                        let new_name = format!("{latest_release_version}-x64-direct-openhcl.bin");
                        fs_err::copy(src, test_content_dir.join(new_name))?;
                    }
                }

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

                let uefi_dir = test_content_dir
                    .join(format!(
                        "hyperv.uefi.mscoreuefi.{}.RELEASE",
                        match mu_msvm_arch {
                            MuMsvmArch::Aarch64 => "AARCH64",
                            MuMsvmArch::X86_64 => "x64",
                        }
                    ))
                    .join(format!(
                        "Msvm{}",
                        match mu_msvm_arch {
                            MuMsvmArch::Aarch64 => "AARCH64",
                            MuMsvmArch::X86_64 => "X64",
                        }
                    ))
                    .join("RELEASE_VS2022")
                    .join("FV");
                fs_err::create_dir_all(&uefi_dir)?;
                fs_err::copy(uefi, uefi_dir.join("MSVM.fd"))?;

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
