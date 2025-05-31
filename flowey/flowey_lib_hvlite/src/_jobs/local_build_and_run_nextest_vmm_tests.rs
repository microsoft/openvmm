// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that builds everything needed and runs the VMM tests

use crate::_jobs::local_build_igvm::non_production_build_igvm_tool_out_name;
use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonPlatform;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::str::FromStr;
use vmm_test_images::KnownTestArtifacts;

#[derive(Serialize, Deserialize)]
pub enum VmmTestSelections {
    Custom {
        /// Custom test filter
        filter: String,
        /// Custom list of artifacts to download
        artifacts: Vec<KnownTestArtifacts>,
        /// Custom list of artifacts to build
        build: BuildSelections,
    },
    Flags(VmmTestSelectionFlags),
}

/// Define VMM test selection flags
macro_rules! define_vmm_test_selection_flags {
    {
        $(
            $name:ident: $default_value:literal,
        )*
    } => {
        #[derive(Serialize, Deserialize, Clone)]
        pub struct VmmTestSelectionFlags {
            $(
                pub $name: bool,
            )*
        }

        impl Default for VmmTestSelectionFlags {
            fn default() -> Self {
                Self {
                    $(
                        $name: $default_value,
                    )*
                }
            }
        }

        impl FromStr for VmmTestSelectionFlags {
            type Err = anyhow::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let mut flags = Self::default();
                for flag in s.split(',') {
                    let (sign, flag) = flag.split_at_checked(1).context("get sign")?;
                    let val = match sign {
                        "+" => true,
                        "-" => false,
                        s => anyhow::bail!("invalid sign: {s}"),
                    };
                    match flag {
                        $(
                            stringify!($name) => flags.$name = val,
                        )*
                        f => anyhow::bail!("invalid flag: {f}"),
                    }
                }
                Ok(flags)
            }
        }
    };
}

define_vmm_test_selection_flags! {
    tdx: false,
    hyperv_vbs: false,
    windows: true,
    ubuntu: true,
    freebsd: true,
    openhcl: true,
    openvmm: true,
    hyperv: true,
    uefi: true,
    pcat: true,
    tmk: true,
    guest_test_uefi: true,
}

#[derive(Serialize, Deserialize)]
pub struct BuildSelections {
    pub openhcl: bool,
    pub openvmm: bool,
    pub pipette_windows: bool,
    pub pipette_linux: bool,
    pub guest_test_uefi: bool,
    pub tmks: bool,
    pub tmk_vmm_windows: bool,
    pub tmk_vmm_linux: bool,
}

// Build everything we can by default
impl Default for BuildSelections {
    fn default() -> Self {
        Self {
            openhcl: true,
            openvmm: true,
            pipette_windows: true,
            pipette_linux: true,
            guest_test_uefi: true,
            tmks: true,
            tmk_vmm_windows: true,
            tmk_vmm_linux: true,
        }
    }
}

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,

        pub test_content_dir: Option<PathBuf>,

        pub selections: VmmTestSelections,

        /// Use unstable WHP interfaces
        pub unstable_whp: bool,
        /// Release build instead of debug build
        pub release: bool,

        /// Whether to run the tests or just build and archive
        pub build_only: bool,
        /// Copy extras to output dir (symbols, etc)
        pub copy_extras: bool,

        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_guest_test_uefi::Node>();
        ctx.import::<crate::build_nextest_vmm_tests::Node>();
        ctx.import::<crate::build_openhcl_igvm_from_recipe::Node>();
        ctx.import::<crate::build_openvmm::Node>();
        ctx.import::<crate::build_pipette::Node>();
        ctx.import::<crate::build_tmks::Node>();
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            target,
            test_content_dir,
            selections,
            unstable_whp,
            release,
            build_only,
            copy_extras,
            done,
        } = request;

        let arch = target.common_arch().unwrap();
        let arch_tag = match arch {
            CommonArch::X86_64 => "x64",
            CommonArch::Aarch64 => "aarch64",
        };
        let platform_tag = match target.as_triple().operating_system {
            target_lexicon::OperatingSystem::Windows => "windows",
            target_lexicon::OperatingSystem::Linux => "linux",
            _ => unreachable!(),
        };
        let test_label = format!("{arch_tag}-{platform_tag}-vmm-tests");

        // Some things can only be built on linux
        let linux_host = matches!(ctx.platform(), FlowPlatform::Linux(_));

        let mut copy_to_dir = Vec::new();
        let extras_dir = Path::new("extras");

        let (nextest_filter_expr, test_artifacts, mut build) = match selections {
            VmmTestSelections::Custom {
                filter,
                artifacts,
                build,
            } => (filter, artifacts, build),
            VmmTestSelections::Flags(VmmTestSelectionFlags {
                tdx,
                hyperv_vbs,
                windows,
                mut ubuntu,
                freebsd,
                mut openhcl,
                openvmm,
                hyperv,
                uefi,
                pcat,
                tmk,
                guest_test_uefi,
            }) => {
                let mut build = BuildSelections::default();

                if !linux_host {
                    log::warn!(
                        "Cannot build for linux on windows. Skipping all tests that rely on linux artifacts."
                    );
                    ubuntu = false;
                    openhcl = false;
                }

                // VTL2 not supported on Linux
                if !matches!(
                    target.as_triple().operating_system,
                    target_lexicon::OperatingSystem::Windows
                ) {
                    openhcl = false;
                }

                let mut filter = "all()".to_string();
                if !tdx {
                    filter.push_str(" & !test(tdx)");
                }
                if !hyperv_vbs {
                    filter.push_str(" & !(test(vbs) & test(hyperv))");
                }
                if !ubuntu {
                    filter.push_str(" & !test(ubuntu)");
                    build.pipette_linux = false;
                }
                if !windows {
                    filter.push_str(" & !test(windows)");
                    build.pipette_windows = false;
                }
                if !freebsd {
                    filter.push_str(" & !test(freebsd)");
                }
                if !openhcl {
                    filter.push_str(" & !test(openhcl)");
                    build.openhcl = false;
                }
                if !openvmm {
                    filter.push_str(" & !test(openvmm)");
                    build.openvmm = false;
                }
                if !hyperv {
                    filter.push_str(" & !test(hyperv)");
                }
                if !uefi {
                    filter.push_str(" & !test(uefi)");
                }
                if !pcat {
                    filter.push_str(" & !test(pcat)");
                }
                if !tmk {
                    filter.push_str(" & !test(tmk)");
                    build.tmks = false;
                    build.tmk_vmm_linux = false;
                    build.tmk_vmm_windows = false;
                }
                if !guest_test_uefi {
                    filter.push_str(" & !test(guest_test_uefi)");
                    build.guest_test_uefi = false;
                }

                let artifacts = match arch {
                    CommonArch::X86_64 => {
                        let mut artifacts = Vec::new();

                        if windows && (tdx || hyperv_vbs) {
                            artifacts.push(KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd);
                        }
                        if ubuntu {
                            artifacts.push(KnownTestArtifacts::Ubuntu2204ServerX64Vhd);
                        }
                        if windows {
                            artifacts.extend_from_slice(&[
                                KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd,
                                KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd,
                            ]);
                        }
                        if freebsd {
                            artifacts.extend_from_slice(&[
                                KnownTestArtifacts::FreeBsd13_2X64Vhd,
                                KnownTestArtifacts::FreeBsd13_2X64Iso,
                            ]);
                        }
                        if windows || ubuntu {
                            artifacts.push(KnownTestArtifacts::VmgsWithBootEntry);
                        }

                        artifacts
                    }
                    CommonArch::Aarch64 => {
                        let mut artifacts = Vec::new();

                        if ubuntu {
                            artifacts.push(KnownTestArtifacts::Ubuntu2404ServerAarch64Vhd);
                        }
                        if windows {
                            artifacts.push(KnownTestArtifacts::Windows11EnterpriseAarch64Vhdx);
                        }
                        if windows || ubuntu {
                            artifacts.push(KnownTestArtifacts::VmgsWithBootEntry);
                        }

                        artifacts
                    }
                };

                (filter, artifacts, build)
            }
        };

        if !linux_host {
            build.openhcl = false;
            build.pipette_linux = false;
            build.tmk_vmm_linux = false;
        }

        let register_openhcl_igvm_files = build.openhcl.then(|| {
            let openvmm_hcl_profile = if release {
                OpenvmmHclBuildProfile::OpenvmmHclShip
            } else {
                OpenvmmHclBuildProfile::Debug
            };
            let openhcl_recipies = match arch {
                CommonArch::X86_64 => vec![
                    OpenhclIgvmRecipe::X64,
                    OpenhclIgvmRecipe::X64Devkern,
                    OpenhclIgvmRecipe::X64TestLinuxDirect,
                    OpenhclIgvmRecipe::X64Cvm,
                ],
                CommonArch::Aarch64 => {
                    vec![
                        OpenhclIgvmRecipe::Aarch64,
                        OpenhclIgvmRecipe::Aarch64Devkern,
                    ]
                }
            };
            let openhcl_extras_dir = extras_dir.join("openhcl");

            let mut register_openhcl_igvm_files = Vec::new();
            for recipe in openhcl_recipies {
                let (read_built_openvmm_hcl, built_openvmm_hcl) = ctx.new_var();
                let (read_built_openhcl_igvm, built_openhcl_igvm) = ctx.new_var();
                let (read_built_openhcl_boot, built_openhcl_boot) = ctx.new_var();
                let (read_built_sidecar, built_sidecar) = ctx.new_var();
                ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                    profile: openvmm_hcl_profile,
                    recipe: recipe.clone(),
                    custom_target: None,
                    built_openvmm_hcl,
                    built_openhcl_boot,
                    built_openhcl_igvm,
                    built_sidecar,
                });

                register_openhcl_igvm_files.push(read_built_openhcl_igvm.map(ctx, {
                    let recipe = recipe.clone();
                    |x| (recipe, x)
                }));

                if copy_extras {
                    let dir =
                        openhcl_extras_dir.join(non_production_build_igvm_tool_out_name(&recipe));
                    copy_to_dir.extend_from_slice(&[
                        (
                            dir.clone(),
                            read_built_openvmm_hcl.map(ctx, |x| Some(x.bin)),
                        ),
                        (dir.clone(), read_built_openvmm_hcl.map(ctx, |x| x.dbg)),
                        (
                            dir.clone(),
                            read_built_openhcl_boot.map(ctx, |x| Some(x.bin)),
                        ),
                        (
                            dir.clone(),
                            read_built_openhcl_boot.map(ctx, |x| Some(x.dbg)),
                        ),
                        (
                            dir.clone(),
                            read_built_sidecar.map(ctx, |x| x.map(|y| y.bin)),
                        ),
                        (
                            dir.clone(),
                            read_built_sidecar.map(ctx, |x| x.map(|y| y.dbg)),
                        ),
                    ]);
                }
            }
            let register_openhcl_igvm_files: ReadVar<
                Vec<(OpenhclIgvmRecipe, crate::run_igvmfilegen::IgvmOutput)>,
            > = ReadVar::transpose_vec(ctx, register_openhcl_igvm_files);

            register_openhcl_igvm_files
        });

        let register_openvmm = build.openvmm.then(|| {
            let output = ctx.reqv(|v| crate::build_openvmm::Request {
                params: crate::build_openvmm::OpenvmmBuildParams {
                    target: target.clone(),
                    profile: CommonProfile::from_release(release),
                    // FIXME: this relies on openvmm default features
                    features: if unstable_whp {
                        [crate::build_openvmm::OpenvmmFeature::UnstableWhp].into()
                    } else {
                        [].into()
                    },
                },
                openvmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_openvmm::OpenvmmOutput::WindowsBin { exe: _, pdb } => pdb,
                            crate::build_openvmm::OpenvmmOutput::LinuxBin { bin: _, dbg } => dbg,
                        })
                    }),
                ));
            }
            output
        });

        let register_pipette_windows = build.pipette_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_pipette::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                pipette: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_pipette::PipetteOutput::WindowsBin { exe: _, pdb } => pdb,
                            _ => unreachable!(),
                        })
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

        let register_tmk_vmm = build.tmk_vmm_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_tmk_vmm::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                unstable_whp,
                profile: CommonProfile::from_release(release),
                tmk_vmm: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_tmk_vmm::TmkVmmOutput::WindowsBin { exe: _, pdb } => pdb,
                            _ => unreachable!(),
                        })
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
                unstable_whp,
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

        let nextest_archive_file = ctx.reqv(|v| crate::build_nextest_vmm_tests::Request {
            target: target.as_triple(),
            profile: CommonProfile::from_release(release),
            build_mode: crate::build_nextest_vmm_tests::BuildNextestVmmTestsMode::Archive(v),
        });
        let nextest_archive_path = Path::new("vmm-tests-archive.tar.zst");
        copy_to_dir.push((
            nextest_archive_path.to_owned(),
            nextest_archive_file.map(ctx, |x| Some(x.archive_file)),
        ));

        if let Some(dir) = &test_content_dir {
            let vmm_test_artifacts_dir = dir.join("images");
            fs_err::create_dir_all(&vmm_test_artifacts_dir)?;
            ctx.req(
                crate::download_openvmm_vmm_tests_artifacts::Request::CustomCacheDir(
                    vmm_test_artifacts_dir,
                ),
            );
        }
        ctx.req(crate::download_openvmm_vmm_tests_artifacts::Request::Download(test_artifacts));
        let test_artifacts_dir =
            ctx.reqv(crate::download_openvmm_vmm_tests_artifacts::Request::GetDownloadFolder);

        let test_content_dir = test_content_dir
            .map(|x| ReadVar::from_static(x))
            .unwrap_or_else(|| {
                ctx.emit_rust_stepv("creating new test content dir", |_| {
                    |_| Ok(std::env::current_dir()?.absolute()?)
                })
            });

        // use the copied archive file
        let nextest_archive_path = nextest_archive_path.to_owned();
        let nextest_archive_file = test_content_dir.map(ctx, |dir| NextestVmmTestsArchive {
            archive_file: dir.join(nextest_archive_path),
        });

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let nextest_config_file = Path::new("nextest.toml");
        let nextest_config_file_src = openvmm_repo_path.map(ctx, move |p| {
            Some(p.join(".config").join(nextest_config_file))
        });
        copy_to_dir.push((nextest_config_file.to_owned(), nextest_config_file_src));
        let nextest_config_file =
            test_content_dir.map(ctx, move |dir| dir.join(nextest_config_file));

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

        let target = target.as_triple();
        let nextest_bin = Path::new(match target.operating_system {
            target_lexicon::OperatingSystem::Windows => "cargo-nextest.exe",
            _ => "cargo-nextest",
        });
        let nextest_bin_src = ctx
            .reqv(|v| {
                flowey_lib_common::download_cargo_nextest::Request::Get(
                    ReadVar::from_static(target.clone()),
                    v,
                )
            })
            .map(ctx, Some);
        copy_to_dir.push((nextest_bin.to_owned(), nextest_bin_src));
        let nextest_bin = test_content_dir.map(ctx, move |dir| dir.join(nextest_bin));

        let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
            test_content_dir: test_content_dir.clone(),
            vmm_tests_target: target.clone(),
            register_openvmm,
            register_pipette_windows,
            register_pipette_linux_musl,
            register_guest_test_uefi,
            register_tmks,
            register_tmk_vmm,
            register_tmk_vmm_linux_musl,
            disk_images_dir: Some(test_artifacts_dir),
            register_openhcl_igvm_files,
            get_test_log_path: None,
            get_env: v,
        });

        let copied_files = ctx.emit_rust_step("copy additional files to test content dir", |ctx| {
            let copy_to_dir = copy_to_dir
                .into_iter()
                .map(|(dst, src)| (dst, src.claim(ctx)))
                .collect::<Vec<_>>();
            let test_content_dir = test_content_dir.clone().claim(ctx);

            move |rt| {
                let test_content_dir = rt.read(test_content_dir);

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
        });

        if build_only {
            ctx.emit_side_effect_step(
                [
                    nextest_archive_file.into_side_effect(),
                    nextest_config_file.into_side_effect(),
                    nextest_bin.into_side_effect(),
                    extra_env.into_side_effect(),
                    copied_files.into_side_effect(),
                ],
                [done],
            );
        } else {
            let results = ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request {
                nextest_archive_file,
                nextest_profile: crate::run_cargo_nextest_run::NextestProfile::Default,
                nextest_filter_expr: Some(nextest_filter_expr),
                nextest_working_dir: Some(test_content_dir.clone()),
                nextest_config_file: Some(nextest_config_file),
                nextest_bin: Some(nextest_bin),
                target: Some(ReadVar::from_static(target)),
                extra_env,
                pre_run_deps: vec![copied_files],
                results: v,
            });

            let junit_xml = results.map(ctx, |r| r.junit_xml);
            let published_results =
                ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
                    junit_xml,
                    test_label,
                    attachments: BTreeMap::new(), // the logs are already there
                    output_dir: Some(test_content_dir),
                    done: v,
                });

            ctx.emit_rust_step("report test results", |ctx| {
                published_results.claim(ctx);
                done.claim(ctx);

                let results = results.clone().claim(ctx);
                move |rt| {
                    let results = rt.read(results);
                    if results.all_tests_passed {
                        log::info!("all tests passed!");
                    } else {
                        log::error!("encountered test failures.");
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
