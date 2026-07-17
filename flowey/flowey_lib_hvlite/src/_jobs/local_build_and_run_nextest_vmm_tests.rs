// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that builds everything needed and runs the VMM tests

use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeDetailsLocalOnly;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeType;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonProfile;
use crate::common::CommonTriple;
use crate::install_vmm_tests_deps::VmmTestsDepSelections;
use flowey::node::prelude::*;
use flowey_lib_common::gen_cargo_nextest_run_cmd::CommandShell;
use flowey_lib_common::gen_cargo_nextest_run_cmd::RunKindDeps;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use vmm_test_images::KnownTestArtifacts;

#[derive(Serialize, Deserialize)]
pub struct VmmTestSelections {
    /// Test filter
    pub filter: String,
    /// List of artifacts to download
    pub artifacts: Vec<KnownTestArtifacts>,
    /// List of artifacts to build
    pub build: BuildSelections,
    /// Dependencies to install
    pub deps: VmmTestsDepSelections,
    /// Whether to download release IGVM files from GitHub
    pub needs_release_igvm: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BuildSelections {
    pub openhcl_standard: bool,
    pub openhcl_standard_dev: bool,
    pub openhcl_cvm: bool,
    pub openhcl_linux_direct: bool,
    pub openvmm: bool,
    pub openvmm_vhost: bool,
    pub pipette_windows: bool,
    pub pipette_linux: bool,
    pub prep_steps_standard: bool,
    pub prep_steps_no_vmbus: bool,
    pub guest_test_uefi: bool,
    pub tmks: bool,
    pub tmk_vmm_windows: bool,
    pub tmk_vmm_linux: bool,
    pub vmgstool: bool,
    pub vmgstool_dev: bool,
    pub tpm_guest_tests_windows: bool,
    pub tpm_guest_tests_linux: bool,
    pub test_igvm_agent_rpc_server: bool,
}

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,

        /// Toolchain platform to use when cross-compiling Windows *guest*
        /// payloads (e.g. pipette). On a non-WSL Linux build host this is
        /// [`CommonPlatform::WindowsGnu`], since the MSVC toolchain is
        /// unavailable there; otherwise it is [`CommonPlatform::WindowsMsvc`].
        pub windows_guest_platform: CommonPlatform,

        pub test_content_dir: PathBuf,

        pub selections: VmmTestSelections,

        /// Release build instead of debug build
        pub release: bool,

        /// Whether to run the tests or just build and archive
        pub build_only: bool,
        /// Copy extras to output dir (symbols, etc)
        pub copy_extras: bool,

        /// Optional: provide a custom kernel modules cpio or directory for initrd layering
        pub custom_kernel_modules: Option<PathBuf>,
        /// Optional: provide a custom kernel image to embed in IGVM (forces UEFI)
        pub custom_kernel: Option<PathBuf>,

        /// Skip the interactive VHD download prompt
        pub skip_vhd_prompt: bool,

        pub nextest_profile: crate::run_cargo_nextest_run::NextestProfile,

        pub reuse_prepped_vhds: bool,

        pub disable_secure_avic: bool,

        /// Optional: incubator profile path. When set, tests run inside
        /// an emulated VM instead of on the host.
        pub incubator_profile: Option<PathBuf>,

        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_guest_test_uefi::Node>();
        ctx.import::<crate::build_incubator::Node>();
        ctx.import::<crate::build_nextest_vmm_tests::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_openvmm_vhost::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::build_prep_steps::Node>();
        ctx.import::<crate::build_tmks::Node>();
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::build_tpm_guest_tests::Node>();
        ctx.import::<crate::build_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
        ctx.import::<crate::run_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::stop_test_igvm_agent_rpc_server::Node>();
        ctx.import::<crate::download_release_igvm_files_from_gh::resolve::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::gen_cargo_nextest_run_cmd::Node>();
        ctx.import::<crate::install_vmm_tests_deps::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::resolve_openvmm_qemu::Node>();
        ctx.import::<crate::run_prep_steps::Node>();
        ctx.import::<crate::build_vmgstool::Node>();
        ctx.import::<crate::write_incubator_target_runner::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            windows_guest_platform,
            test_content_dir,
            selections,
            release,
            build_only,
            copy_extras,
            custom_kernel_modules,
            custom_kernel,
            skip_vhd_prompt,
            nextest_profile,
            reuse_prepped_vhds,
            disable_secure_avic,
            incubator_profile,
            done,
        } = request;

        let test_content_dir = test_content_dir.absolute()?;
        let custom_kernel_modules_abs = custom_kernel_modules.map(|p| p.absolute()).transpose()?;
        let custom_kernel_abs = custom_kernel.map(|p| p.absolute()).transpose()?;

        let target_triple = target.as_triple();
        let arch = target.common_arch().unwrap();
        let arch_tag = match arch {
            CommonArch::X86_64 => "x64",
            CommonArch::Aarch64 => "aarch64",
        };
        let platform_tag = match target_triple.operating_system {
            target_lexicon::OperatingSystem::Windows => "windows",
            target_lexicon::OperatingSystem::Linux => "linux",
            _ => unreachable!(),
        };
        let test_label = format!("{arch_tag}-{platform_tag}-vmm-tests");

        let mut copy_to_dir = Vec::new();
        let extras_dir = Path::new("extras");

        let VmmTestSelections {
            filter: nextest_filter_expr,
            artifacts: test_artifacts,
            build,
            deps,
            needs_release_igvm,
        } = selections;

        let build_openhcl = build.openhcl_standard
            || build.openhcl_standard_dev
            || build.openhcl_cvm
            || build.openhcl_linux_direct;

        // Some things can only be built on linux
        if !matches!(ctx.platform(), FlowPlatform::Linux(_))
            && (build_openhcl
                || build.pipette_linux
                || build.openvmm_vhost
                || build.tmk_vmm_linux
                || build.tpm_guest_tests_linux)
        {
            anyhow::bail!(
                "Selected tests require artifacts that can only be built on linux. Try building from WSL2."
            );
        }

        let register_openhcl_igvm_files = if build_openhcl {
            let openvmm_hcl_profile = if release {
                OpenvmmHclBuildProfile::OpenvmmHclShip
            } else {
                OpenvmmHclBuildProfile::Debug
            };
            let mut openhcl_recipes = Vec::new();
            match arch {
                CommonArch::X86_64 => {
                    if build.openhcl_standard {
                        openhcl_recipes.push(OpenhclIgvmRecipe::X64);
                    }
                    if build.openhcl_standard_dev {
                        openhcl_recipes.push(OpenhclIgvmRecipe::X64Devkern);
                    }
                    if build.openhcl_cvm {
                        openhcl_recipes.push(OpenhclIgvmRecipe::X64Cvm);
                    }
                    if build.openhcl_linux_direct {
                        openhcl_recipes.push(OpenhclIgvmRecipe::X64TestLinuxDirect);
                    }
                }
                CommonArch::Aarch64 => {
                    if build.openhcl_standard {
                        openhcl_recipes.push(OpenhclIgvmRecipe::Aarch64);
                    }
                    if build.openhcl_standard_dev {
                        openhcl_recipes.push(OpenhclIgvmRecipe::Aarch64Devkern);
                    }
                }
            };
            let openhcl_extras_dir = extras_dir.join("openhcl");

            let mut register_openhcl_igvm_files = Vec::new();
            for recipe in openhcl_recipes {
                let (read_openhcl_igvm, openhcl_igvm) = ctx.new_var();
                let (read_openhcl_igvm_extras, openhcl_igvm_extras) = ctx.new_var();

                let recipe_to_use =
                    if custom_kernel_modules_abs.is_some() || custom_kernel_abs.is_some() {
                        let mut details = recipe.recipe_details(release);
                        if custom_kernel_abs.is_some() {
                            details.with_uefi = true;
                        }
                        assert!(details.local_only.is_none());
                        details.local_only = Some(OpenhclIgvmRecipeDetailsLocalOnly {
                            openvmm_hcl_no_strip: false,
                            openhcl_initrd_extra_params: None,
                            custom_openvmm_hcl: None,
                            custom_openhcl_boot: None,
                            custom_kernel: custom_kernel_abs.clone(),
                            custom_sidecar: None,
                            custom_extra_rootfs: vec![],
                        });
                        OpenhclIgvmRecipeType::LocalOnlyCustom(details)
                    } else {
                        OpenhclIgvmRecipeType::WellKnown(recipe.clone())
                    };

                ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                    build_profile: openvmm_hcl_profile,
                    release_cfg: release,
                    recipe: recipe_to_use,
                    custom_target: None,
                    extra_features: BTreeSet::new(),
                    disable_secure_avic,
                    confidential_debug: true,
                    openhcl_igvm,
                    openhcl_igvm_extras,
                });

                register_openhcl_igvm_files.push(read_openhcl_igvm);

                if copy_extras {
                    let dir = openhcl_extras_dir.join(recipe.non_production_tag());
                    copy_to_dir.extend_from_slice(&[
                        (
                            dir.clone(),
                            read_openhcl_igvm_extras.map(ctx, |x| Some(x.openvmm_hcl.bin)),
                        ),
                        (
                            dir.clone(),
                            read_openhcl_igvm_extras.map(ctx, |x| x.openvmm_hcl.dbg),
                        ),
                        (
                            dir.clone(),
                            read_openhcl_igvm_extras.map(ctx, |x| Some(x.openhcl_boot.bin)),
                        ),
                        (
                            dir.clone(),
                            read_openhcl_igvm_extras.map(ctx, |x| Some(x.openhcl_boot.dbg)),
                        ),
                        (
                            dir.clone(),
                            read_openhcl_igvm_extras.map(ctx, |x| x.sidecar.map(|y| y.bin)),
                        ),
                        (
                            dir.clone(),
                            read_openhcl_igvm_extras.map(ctx, |x| x.sidecar.map(|y| y.dbg)),
                        ),
                    ]);
                } else {
                    read_openhcl_igvm_extras.claim_unused(ctx);
                }
            }

            register_openhcl_igvm_files
        } else {
            Vec::new()
        };

        let register_openvmm = build.openvmm.then(|| {
            let output = ctx.reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    target: target.clone(),
                    profile: CommonProfile::from_release(release),
                    // FIXME: this relies on openvmm default features
                    features: [].into(),
                },
                version: None,
                openvmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_openvmm::OpenvmmOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_openvmm::OpenvmmOutput::LinuxBin { bin: _, dbg } => Some(dbg),
                    }),
                ));
            }
            output
        });

        let register_openvmm_vhost = build.openvmm_vhost.then(|| {
            ctx.reqv(|v| crate::build_openvmm_vhost::Request {
                params: crate::build_openvmm_vhost::OpenvmmVhostBuildParams {
                    target: target.clone(),
                    profile: CommonProfile::from_release(release),
                },
                openvmm_vhost: v,
            })
        });

        let register_pipette_windows = build.pipette_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_pipette::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: windows_guest_platform,
                },
                profile: CommonProfile::from_release(release),
                pipette: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_pipette::PipetteOutput::WindowsBin { exe: _, pdb } => pdb,
                        _ => unreachable!(),
                    }),
                ));
            }
            output
        });

        let register_pipette_linux_musl = build.pipette_linux.then(|| {
            let output = ctx.reqv(|v| crate::build_pipette::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::LinuxMusl,
                },
                profile: CommonProfile::from_release(release),
                pipette: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_pipette::PipetteOutput::LinuxBin { bin: _, dbg } => dbg,
                            _ => unreachable!(),
                        })
                    }),
                ));
            }
            output
        });

        let register_guest_test_uefi = build.guest_test_uefi.then(|| {
            let output = ctx.reqv(|v| crate::build_guest_test_uefi::Request {
                arch,
                profile: CommonProfile::from_release(release),
                guest_test_uefi: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| Some(x.efi))));
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| Some(x.pdb))));
            }
            output
        });

        let register_tmks = build.tmks.then(|| {
            let output = ctx.reqv(|v| crate::build_tmks::Request {
                arch,
                profile: CommonProfile::from_release(release),
                tmks: v,
            });
            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| Some(x.dbg))));
            }
            output
        });

        let register_tpm_guest_tests_windows = build.tpm_guest_tests_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_tpm_guest_tests::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: windows_guest_platform,
                },
                profile: CommonProfile::from_release(release),
                tpm_guest_tests: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        TpmGuestTestsOutput::WindowsBin { pdb, .. } => pdb.clone(),
                        TpmGuestTestsOutput::LinuxBin { .. } => unreachable!(),
                    }),
                ));
            }
            output
        });

        let register_tpm_guest_tests_linux = build.tpm_guest_tests_linux.then(|| {
            let output = ctx.reqv(|v| crate::build_tpm_guest_tests::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::LinuxGnu,
                },
                profile: CommonProfile::from_release(release),
                tpm_guest_tests: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            TpmGuestTestsOutput::LinuxBin { dbg, .. } => dbg.clone(),
                            TpmGuestTestsOutput::WindowsBin { .. } => unreachable!(),
                        })
                    }),
                ));
            }
            output
        });

        let register_test_igvm_agent_rpc_server = build.test_igvm_agent_rpc_server.then(|| {
            let output = ctx.reqv(|v| crate::build_test_igvm_agent_rpc_server::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                test_igvm_agent_rpc_server: v,
            });

            if copy_extras {
                copy_to_dir.push((extras_dir.to_owned(), output.map(ctx, |x| x.pdb.clone())));
            }
            output
        });

        let register_tmk_vmm = build.tmk_vmm_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                tmk_vmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_tmk_vmm::TmkVmmOutput::WindowsBin { exe: _, pdb } => pdb,
                        _ => unreachable!(),
                    }),
                ));
            }
            output
        });

        let register_tmk_vmm_linux_musl = build.tmk_vmm_linux.then(|| {
            let output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::LinuxMusl,
                },
                profile: CommonProfile::from_release(release),
                tmk_vmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_tmk_vmm::TmkVmmOutput::LinuxBin { bin: _, dbg } => dbg,
                            _ => unreachable!(),
                        })
                    }),
                ));
            }
            output
        });

        let needs_prep_steps = build.prep_steps_standard || build.prep_steps_no_vmbus;
        let mut prep_steps_variants: Vec<String> = Vec::new();
        if build.prep_steps_standard {
            prep_steps_variants.push("standard".into());
        }
        if build.prep_steps_no_vmbus {
            prep_steps_variants.push("no-vmbus".into());
        }

        let register_prep_steps = needs_prep_steps.then(|| {
            let (prep_steps_bin, platform) = match target_triple.operating_system {
                target_lexicon::OperatingSystem::Windows => {
                    (Path::new("prep_steps.exe"), CommonPlatform::WindowsMsvc)
                }
                target_lexicon::OperatingSystem::Linux => {
                    (Path::new("prep_steps"), CommonPlatform::LinuxGnu)
                }
                _ => unreachable!(),
            };

            let output = ctx.reqv(|v| crate::build_prep_steps::Request {
                target: CommonTriple::Common { arch, platform },
                profile: CommonProfile::from_release(release),
                prep_steps: v,
            });

            copy_to_dir.push((
                prep_steps_bin.to_owned(),
                output.map(ctx, |x| {
                    Some(match x {
                        crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe, pdb: _ } => exe,
                        crate::build_prep_steps::PrepStepsOutput::LinuxBin { bin, dbg: _ } => bin,
                    })
                }),
            ));
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_prep_steps::PrepStepsOutput::LinuxBin { bin: _, dbg } => {
                            Some(dbg)
                        }
                    }),
                ));
            }

            let is_windows_target = matches!(
                target_triple.operating_system,
                target_lexicon::OperatingSystem::Windows
            );
            let cmds: Vec<(std::ffi::OsString, Vec<std::ffi::OsString>)> = prep_steps_variants
                .iter()
                .map(|variant| {
                    let cmd_path = if is_windows_target {
                        format!("$PSScriptRoot\\{}", prep_steps_bin.to_string_lossy())
                    } else {
                        format!("./{}", prep_steps_bin.to_string_lossy())
                    };
                    (cmd_path.into(), vec![variant.clone().into()])
                })
                .collect();

            let prep_steps_bin = test_content_dir.join(prep_steps_bin);
            let output = output.map(ctx, |mut output| {
                let path = match &mut output {
                    crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe, pdb: _ } => exe,
                    crate::build_prep_steps::PrepStepsOutput::LinuxBin { bin, dbg: _ } => bin,
                };
                *path = prep_steps_bin;
                output
            });

            (output, cmds)
        });

        let mut build_vmgstool = |with_test_helpers| {
            let output = ctx.reqv(|v| crate::build_vmgstool::Request {
                target: target.clone(),
                profile: CommonProfile::from_release(release),
                with_crypto: true,
                with_test_helpers,
                vmgstool: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| match x {
                        crate::build_vmgstool::VmgstoolOutput::WindowsBin { exe: _, pdb } => pdb,
                        crate::build_vmgstool::VmgstoolOutput::LinuxBin { bin: _, dbg } => {
                            Some(dbg)
                        }
                    }),
                ));
            }
            output
        };

        let register_vmgstool = build.vmgstool.then(|| build_vmgstool(false));

        let register_vmgstool_dev = build.vmgstool_dev.then(|| build_vmgstool(true));

        let nextest_archive = ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
            target: target.as_triple(),
            profile: CommonProfile::from_release(release),
            build_mode: crate::build_nextest_vmm_tests::BuildNextestVmmTestsMode::Archive(v),
        });
        let nextest_archive_file = Path::new("vmm-tests-archive.tar.zst");
        copy_to_dir.push((
            nextest_archive_file.to_owned(),
            nextest_archive.map(ctx, |x| Some(x.archive_file)),
        ));

        let vmm_test_artifacts_dir = test_content_dir.join("images");
        fs_err::create_dir_all(&vmm_test_artifacts_dir)?;
        ctx.config(crate::download_openvmm_vmm_tests_artifacts::Config {
            custom_cache_dir: Some(vmm_test_artifacts_dir),
            skip_prompt: Some(skip_vhd_prompt),
            ..Default::default()
        });

        ctx.req(crate::download_openvmm_vmm_tests_artifacts::Request::Download(test_artifacts));
        let test_artifacts_dir =
            ctx.reqv(crate::download_openvmm_vmm_tests_artifacts::Request::GetDownloadFolder);

        ctx.config(crate::install_vmm_tests_deps::Config {
            selections: Some(deps),
            auto_install: None,
        });
        let dep_install_cmds = ctx.reqv(crate::install_vmm_tests_deps::Request::GetCommands);

        // use the copied archive file
        let nextest_archive_file = test_content_dir.join(nextest_archive_file);

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let nextest_config_file = Path::new("nextest.toml");
        let nextest_config_file_src = openvmm_repo_path.map(ctx, move |p| {
            Some(p.join(".config").join(nextest_config_file))
        });
        copy_to_dir.push((nextest_config_file.to_owned(), nextest_config_file_src));
        let nextest_config_file = test_content_dir.join(nextest_config_file);

        let cargo_toml_file = Path::new("Cargo.toml");
        let repo_cargo_toml_file_src =
            openvmm_repo_path.map(ctx, move |p| Some(p.join(cargo_toml_file)));
        let crate_cargo_toml_file = PathBuf::new()
            .join("vmm_tests")
            .join("vmm_tests")
            .join(cargo_toml_file);
        let crate_cargo_toml_file_src = crate_cargo_toml_file.clone();
        let crate_cargo_toml_file_src =
            openvmm_repo_path.map(ctx, move |p| Some(p.join(crate_cargo_toml_file_src)));
        copy_to_dir.push((cargo_toml_file.to_owned(), repo_cargo_toml_file_src));
        copy_to_dir.push((crate_cargo_toml_file, crate_cargo_toml_file_src));

        let nextest_bin = Path::new(match target_triple.operating_system {
            target_lexicon::OperatingSystem::Windows => "cargo-nextest.exe",
            _ => "cargo-nextest",
        });
        let nextest_bin_src = ctx
            .reqv(|v| {
                flowey_lib_common::download_cargo_nextest::Request::Get(
                    ReadVar::from_static(target_triple.clone()),
                    v,
                )
            })
            .map(ctx, Some);
        copy_to_dir.push((nextest_bin.to_owned(), nextest_bin_src));
        let nextest_bin = test_content_dir.join(nextest_bin);

        let release_igvm_files = needs_release_igvm.then(|| {
            ctx.reqv(
                |v| crate::download_release_igvm_files_from_gh::resolve::Request {
                    arch,
                    release_igvm_files: v,
                    release_version:
                        crate::download_release_igvm_files_from_gh::OpenhclReleaseVersion::latest(),
                },
            )
        });

        let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
            test_content_dir: ReadVar::from_static(test_content_dir.clone()),
            vmm_tests_target: target_triple.clone(),
            register_openvmm,
            register_openvmm_vhost,
            register_pipette_windows,
            register_pipette_linux_musl,
            register_guest_test_uefi,
            register_tmks,
            register_tmk_vmm,
            register_tmk_vmm_linux_musl,
            register_vmgstool,
            register_vmgstool_dev,
            register_tpm_guest_tests_windows,
            register_tpm_guest_tests_linux,
            register_test_igvm_agent_rpc_server,
            disk_images_dir: Some(test_artifacts_dir),
            register_openhcl_igvm_files,
            get_test_log_path: None,
            get_env: v,
            release_igvm_files,
            use_relative_paths: build_only,
            disable_remote_artifacts: false,
            reuse_prepped_vhds,
        });

        let mut side_effects = Vec::new();

        side_effects.push(
            ctx.emit_rust_step("copy additional files to test content dir", |ctx| {
                let copy_to_dir = copy_to_dir
                    .into_iter()
                    .map(|(dst, src)| (dst, src.claim(ctx)))
                    .collect::<Vec<_>>();
                let test_content_dir = test_content_dir.clone();

                move |rt| {
                    for (dst, src) in copy_to_dir {
                        let src = rt.read(src);

                        if let Some(src) = src {
                            // TODO: specify files names for everything
                            let dst = if dst.starts_with("extras") {
                                test_content_dir
                                    .join(dst)
                                    .join(src.file_name().context("no file name")?)
                            } else {
                                test_content_dir.join(dst)
                            };

                            fs_err::create_dir_all(dst.parent().context("no parent")?)?;
                            fs_err::copy(src, dst)?;
                        }
                    }

                    Ok(())
                }
            }),
        );

        side_effects.push(ctx.emit_rust_step("write dep install script", |ctx| {
            let dep_install_cmds = dep_install_cmds.claim(ctx);
            let test_content_dir = test_content_dir.clone();

            move |rt| {
                let dep_install_cmds = rt.read(dep_install_cmds);

                if !dep_install_cmds.is_empty() {
                    log::info!("Dependency install commands (written to install_deps.ps1):");
                    for cmd in &dep_install_cmds {
                        log::info!("  {cmd}");
                    }
                    let script_contents = dep_install_cmds.join("\n");
                    fs_err::write(test_content_dir.join("install_deps.ps1"), script_contents)?;
                }

                Ok(())
            }
        }));

        let nextest_run_cmd = ctx.reqv(|v| flowey_lib_common::gen_cargo_nextest_run_cmd::Request {
            run_kind_deps: RunKindDeps::RunFromArchive {
                archive_file: ReadVar::from_static(nextest_archive_file.clone()),
                nextest_bin: ReadVar::from_static(nextest_bin.clone()),
                target: ReadVar::from_static(target_triple.clone()),
            },
            working_dir: ReadVar::from_static(test_content_dir.clone()),
            config_file: ReadVar::from_static(nextest_config_file.clone()),
            tool_config_files: Vec::new(),
            nextest_profile: nextest_profile.as_str().to_owned(),
            nextest_filter_expr: Some(nextest_filter_expr.clone()),
            run_ignored: false,
            fail_fast: None,
            extra_env: Some(extra_env.clone()),
            extra_commands: register_prep_steps
                .clone()
                .map(|(_, cmds)| ReadVar::from_static(cmds)),
            portable: true,
            command: v,
        });

        side_effects.push(ctx.emit_rust_step("write test command script", |ctx| {
            let nextest_run_cmd = nextest_run_cmd.claim(ctx);
            let test_content_dir = test_content_dir.clone();

            move |rt| {
                let cmd = rt.read(nextest_run_cmd);

                log::info!("{cmd}");

                let (script_name, script_contents) = match cmd.shell {
                    CommandShell::Powershell => ("run.ps1", cmd.to_string()),
                    CommandShell::Bash => ("run.sh", format!("#!/bin/sh\n{cmd}")),
                };

                fs_err::write(test_content_dir.join(script_name), script_contents)?;

                Ok(())
            }
        }));

        let (extra_env, nextest_bin, nextest_target, stop_rpc_server) = if let Some(profile_path) =
            incubator_profile
        {
            // Incubator mode: host nextest drives the archive, and invokes
            // the generated target runner for each guest test binary.
            if let Some((prep_steps, _)) = register_prep_steps {
                prep_steps.claim_unused(ctx);
            }

            let profile_path = profile_path
                .absolute()
                .context("failed to resolve incubator profile path")?;

            let host_arch = match ctx.arch() {
                FlowArch::X86_64 => CommonArch::X86_64,
                FlowArch::Aarch64 => CommonArch::Aarch64,
                other => {
                    anyhow::bail!("unsupported host architecture for incubator: {other:?}")
                }
            };
            let incubator_target = CommonTriple::Common {
                arch: host_arch,
                platform: CommonPlatform::LinuxGnu,
            };
            let incubator_bin = ctx.reqv(|v| crate::build_incubator::Request {
                target: incubator_target,
                profile: if release {
                    CommonProfile::Release
                } else {
                    CommonProfile::Debug
                },
                incubator: v,
            });

            let incubator_bin = incubator_bin.map(ctx, |o| o.bin);

            let kernel = ctx.reqv(|v| {
                crate::resolve_openvmm_test_linux_kernel::Request::Get(
                    crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
                    arch,
                    crate::resolve_openvmm_test_linux_kernel::INCUBATOR_LINUX_TEST_KERNEL_VERSION,
                    v,
                )
            });
            let initrd = ctx.reqv(|v| crate::resolve_openvmm_test_initrd::Request::Get(arch, v));

            let qemu_binary = ctx.reqv(|v| {
                crate::resolve_openvmm_qemu::Request::Get(
                    crate::resolve_openvmm_qemu::QemuFile::SystemAarch64,
                    host_arch,
                    v,
                )
            });

            let extra_env = ctx.reqv(|v| crate::write_incubator_target_runner::Request {
                incubator_bin,
                profile_path: ReadVar::from_static(profile_path),
                kernel: Some(kernel),
                initrd: Some(initrd),
                repo_root: openvmm_repo_path.clone(),
                test_content_dir: ReadVar::from_static(test_content_dir.clone()),
                extra_share_paths: vec![
                    ReadVar::from_static(nextest_archive_file.clone()),
                    ReadVar::from_static(nextest_config_file.clone()),
                ],
                extra_env: Some(extra_env),
                qemu_binary: Some(qemu_binary),
                target: target_triple.clone(),
                nextest_env: v,
            });

            (extra_env, None, None, false)
        } else if build_only {
            ctx.emit_side_effect_step(side_effects, [done]);
            if let Some((prep_steps, _)) = register_prep_steps {
                prep_steps.claim_unused(ctx);
            }
            return Ok(());
        } else {
            side_effects.push(ctx.reqv(crate::install_vmm_tests_deps::Request::Install));

            // Start the test_igvm_agent_rpc_server before running tests (Windows only).
            let stop_rpc_server = if matches!(ctx.platform(), FlowPlatform::Windows) {
                side_effects.push(ctx.reqv(|done| {
                    crate::run_test_igvm_agent_rpc_server::Request {
                        env: extra_env.clone(),
                        done,
                    }
                }));
                true
            } else {
                false
            };

            if let Some((prep_steps, _)) = register_prep_steps {
                for variant in &prep_steps_variants {
                    side_effects.push(ctx.reqv(|done| crate::run_prep_steps::Request {
                        prep_steps: prep_steps.clone(),
                        args: vec![variant.clone()],
                        env: extra_env.clone(),
                        done,
                    }));
                }
            }

            (
                extra_env,
                Some(ReadVar::from_static(nextest_bin)),
                Some(ReadVar::from_static(target_triple.clone())),
                stop_rpc_server,
            )
        };

        let results = ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request {
            nextest_archive_file: ReadVar::from_static(NextestVmmTestsArchive {
                archive_file: nextest_archive_file,
            }),
            nextest_profile,
            nextest_filter_expr: Some(nextest_filter_expr),
            nextest_working_dir: Some(ReadVar::from_static(test_content_dir.clone())),
            nextest_config_file: Some(ReadVar::from_static(nextest_config_file)),
            nextest_bin,
            target: nextest_target,
            extra_env,
            pre_run_deps: side_effects,
            hugetlb_2mb_overcommit_pages: None,
            results: v,
        });

        let rpc_server_stopped = if stop_rpc_server {
            let after_tests = results.map(ctx, |_| ());
            Some(
                ctx.reqv(|done| crate::stop_test_igvm_agent_rpc_server::Request {
                    after_tests,
                    done,
                }),
            )
        } else {
            None
        };

        let junit_xml = results.map(ctx, |r| r.junit_xml);
        let published_results = ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
            junit_xml,
            test_label,
            attachments: BTreeMap::new(), // the logs are already there
            output_dir: Some(ReadVar::from_static(test_content_dir)),
            done: v,
        });

        ctx.emit_rust_step("report test results", |ctx| {
            published_results.claim(ctx);
            if let Some(rpc_server_stopped) = rpc_server_stopped {
                rpc_server_stopped.claim(ctx);
            }
            done.claim(ctx);

            let results = results.clone().claim(ctx);
            move |rt| {
                let results = rt.read(results);
                if results.all_tests_passed {
                    log::info!("all tests passed!");
                } else {
                    anyhow::bail!("encountered test failures.")
                }

                Ok(())
            }
        });

        Ok(())
    }
}
