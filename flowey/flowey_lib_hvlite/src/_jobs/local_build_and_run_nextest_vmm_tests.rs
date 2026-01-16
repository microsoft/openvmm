// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A local-only job that builds everything needed and runs the VMM tests

use crate::_jobs::local_build_igvm::non_production_build_igvm_tool_out_name;
use crate::build_nextest_vmm_tests::NextestVmmTestsArchive;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipeDetailsLocalOnly;
use crate::build_openhcl_initrd::OpenhclInitrdExtraParams;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_tpm_guest_tests::TpmGuestTestsOutput;
use crate::install_vmm_tests_deps::VmmTestsDepSelections;
use crate::run_cargo_build::common::CommonArch;
use crate::run_cargo_build::common::CommonPlatform;
use crate::run_cargo_build::common::CommonProfile;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;
use flowey_lib_common::gen_cargo_nextest_run_cmd::CommandShell;
use flowey_lib_common::gen_cargo_nextest_run_cmd::RunKindDeps;
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
        /// Dependencies to install
        deps: VmmTestsDepSelections,
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
    tdx: true,
    snp: true,
    hyperv_vbs: true,
    windows: true,
    ubuntu: true,
    freebsd: true,
    linux: true,
    openhcl: true,
    openvmm: true,
    hyperv: true,
    uefi: true,
    pcat: true,
    tmk: true,
    guest_test_uefi: true,
    vmgstool: true,
}

#[derive(Serialize, Deserialize)]
pub struct BuildSelections {
    pub openhcl: bool,
    pub openvmm: bool,
    pub pipette_windows: bool,
    pub pipette_linux: bool,
    pub prep_steps: bool,
    pub guest_test_uefi: bool,
    pub tmks: bool,
    pub tmk_vmm_windows: bool,
    pub tmk_vmm_linux: bool,
    pub vmgstool: bool,
    pub tpm_guest_tests_windows: bool,
    pub tpm_guest_tests_linux: bool,
}

// Build everything we can by default
impl Default for BuildSelections {
    fn default() -> Self {
        Self {
            prep_steps: true,
            openhcl: true,
            openvmm: true,
            pipette_windows: true,
            pipette_linux: true,
            guest_test_uefi: true,
            tmks: true,
            tmk_vmm_windows: true,
            tmk_vmm_linux: true,
            vmgstool: true,
            tpm_guest_tests_windows: true,
            tpm_guest_tests_linux: true,
        }
    }
}

flowey_request! {
    pub struct Params {
        pub target: CommonTriple,

        pub test_content_dir: PathBuf,

        pub selections: VmmTestSelections,

        /// Use unstable WHP interfaces
        pub unstable_whp: bool,
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
        ctx.import::<crate::build_prep_steps::Node>();
        ctx.import::<crate::build_tmks::Node>();
        ctx.import::<crate::build_tmk_vmm::Node>();
        ctx.import::<crate::build_tpm_guest_tests::Node>();
        ctx.import::<crate::download_openvmm_vmm_tests_artifacts::Node>();
        ctx.import::<crate::download_release_igvm_files_from_gh::resolve::Node>();
        ctx.import::<crate::init_vmm_tests_env::Node>();
        ctx.import::<crate::test_nextest_vmm_tests_archive::Node>();
        ctx.import::<flowey_lib_common::publish_test_results::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::download_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::gen_cargo_nextest_run_cmd::Node>();
        ctx.import::<crate::install_vmm_tests_deps::Node>();
        ctx.import::<crate::run_prep_steps::Node>();
        ctx.import::<crate::build_vmgstool::Node>();
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
            custom_kernel_modules,
            custom_kernel,
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

        // Some things can only be built on linux
        let linux_host = matches!(ctx.platform(), FlowPlatform::Linux(_));

        let mut copy_to_dir = Vec::new();
        let extras_dir = Path::new("extras");

        let (nextest_filter_expr, test_artifacts, mut build, deps) = match selections {
            VmmTestSelections::Custom {
                filter,
                artifacts,
                build,
                deps,
            } => (filter, artifacts, build, deps),
            VmmTestSelections::Flags(VmmTestSelectionFlags {
                tdx,
                snp,
                hyperv_vbs,
                windows,
                mut ubuntu,
                freebsd,
                linux,
                mut openhcl,
                openvmm,
                hyperv,
                uefi,
                pcat,
                tmk,
                guest_test_uefi,
                vmgstool,
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
                    target_triple.operating_system,
                    target_lexicon::OperatingSystem::Windows
                ) {
                    openhcl = false;
                }

                let mut filter = "all()".to_string();
                if !tdx {
                    filter.push_str(" & !test(tdx)");
                }
                if !snp {
                    filter.push_str(" & !test(snp)");
                }
                if !hyperv_vbs {
                    filter.push_str(" & !(test(vbs) & test(hyperv))");
                }
                if !ubuntu {
                    filter.push_str(" & !test(ubuntu)");
                }
                if !windows {
                    filter.push_str(" & !test(windows)");
                    build.pipette_windows = false;
                    build.tpm_guest_tests_windows = false;
                }
                if !freebsd {
                    filter.push_str(" & !test(freebsd)");
                }
                if !linux {
                    filter.push_str(" & !test(linux)");
                    build.tpm_guest_tests_linux = false;
                }
                if !linux && !ubuntu {
                    build.pipette_linux = false;
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
                // prep_steps is Windows-only
                if !tdx && !snp && !hyperv_vbs
                    || !matches!(
                        target_triple.operating_system,
                        target_lexicon::OperatingSystem::Windows
                    )
                {
                    build.prep_steps = false;
                }
                if !vmgstool {
                    filter.push_str(" & !test(vmgstool)");
                    build.vmgstool = false;
                }

                let artifacts = match arch {
                    CommonArch::X86_64 => {
                        let mut artifacts = Vec::new();

                        if windows && (tdx || snp || hyperv_vbs) {
                            artifacts.push(KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd);
                            artifacts.push(KnownTestArtifacts::Gen2WindowsDataCenterCore2025X64Vhd);
                        }
                        if ubuntu {
                            artifacts.push(KnownTestArtifacts::Ubuntu2404ServerX64Vhd);
                            artifacts.push(KnownTestArtifacts::Ubuntu2504ServerX64Vhd);
                        }
                        if windows && uefi {
                            artifacts.push(KnownTestArtifacts::Gen2WindowsDataCenterCore2022X64Vhd);
                        }
                        if windows && pcat {
                            artifacts.push(KnownTestArtifacts::Gen1WindowsDataCenterCore2022X64Vhd);
                        }
                        if freebsd && pcat {
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

                let deps = match target_triple.operating_system {
                    target_lexicon::OperatingSystem::Windows => VmmTestsDepSelections::Windows {
                        hyperv,
                        whp: openvmm,
                        hardware_isolation: tdx || snp,
                    },
                    target_lexicon::OperatingSystem::Linux => VmmTestsDepSelections::Linux,
                    _ => unreachable!(),
                };

                (filter, artifacts, build, deps)
            }
        };

        if !linux_host {
            build.openhcl = false;
            build.pipette_linux = false;
            build.tmk_vmm_linux = false;
            build.tpm_guest_tests_linux = false;
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
                let recipe_to_use =
                    if custom_kernel_modules_abs.is_some() || custom_kernel_abs.is_some() {
                        let mut details = recipe.recipe_details(release);
                        if custom_kernel_abs.is_some() {
                            details.with_uefi = true;
                        }
                        assert!(details.local_only.is_none());
                        let initrd_extra =
                            custom_kernel_modules_abs
                                .clone()
                                .map(|ckm| OpenhclInitrdExtraParams {
                                    extra_initrd_layers: vec![],
                                    extra_initrd_directories: vec![],
                                    custom_kernel_modules: Some(ckm),
                                });
                        details.local_only = Some(OpenhclIgvmRecipeDetailsLocalOnly {
                            openvmm_hcl_no_strip: false,
                            openhcl_initrd_extra_params: initrd_extra,
                            custom_openvmm_hcl: None,
                            custom_openhcl_boot: None,
                            custom_uefi: None,
                            custom_kernel: custom_kernel_abs.clone(),
                            custom_sidecar: None,
                            custom_extra_rootfs: vec![],
                        });
                        OpenhclIgvmRecipe::LocalOnlyCustom(details)
                    } else {
                        recipe.clone()
                    };

                ctx.req(crate::build_openhcl_igvm_from_recipe::Request {
                    build_profile: openvmm_hcl_profile,
                    release_cfg: release,
                    recipe: recipe_to_use,
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
                } else {
                    read_built_openvmm_hcl.claim_unused(ctx);
                    read_built_openhcl_boot.claim_unused(ctx);
                    read_built_sidecar.claim_unused(ctx);
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

        let register_tpm_guest_tests_windows = build.tpm_guest_tests_windows.then(|| {
            let output = ctx.reqv(|v| crate::build_tpm_guest_tests::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                tpm_guest_tests: v,
            });

            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            TpmGuestTestsOutput::WindowsBin { pdb, .. } => pdb.clone(),
                            TpmGuestTestsOutput::LinuxBin { .. } => unreachable!(),
                        })
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

        let register_prep_steps = build.prep_steps.then(|| {
            let prep_steps_bin = Path::new(match target_triple.operating_system {
                target_lexicon::OperatingSystem::Windows => "prep_steps.exe",
                _ => unreachable!(),
            });

            let output = ctx.reqv(|v| crate::build_prep_steps::Request {
                target: CommonTriple::Common {
                    arch,
                    platform: CommonPlatform::WindowsMsvc,
                },
                profile: CommonProfile::from_release(release),
                prep_steps: v,
            });

            copy_to_dir.push((
                prep_steps_bin.to_owned(),
                output.map(ctx, |x| {
                    Some(match x {
                        crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe, pdb: _ } => exe,
                        _ => unreachable!(),
                    })
                }),
            ));
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_prep_steps::PrepStepsOutput::WindowsBin {
                                exe: _,
                                pdb,
                            } => pdb,
                            _ => unreachable!(),
                        })
                    }),
                ));
            }

            let cmd = (
                format!("$PSScriptRoot\\{}", prep_steps_bin.to_string_lossy()).into(),
                Vec::new(),
            );

            let prep_steps_bin = test_content_dir.join(prep_steps_bin);
            let output = output.map(ctx, |mut output| {
                let path = match &mut output {
                    crate::build_prep_steps::PrepStepsOutput::WindowsBin { exe, pdb: _ } => exe,
                    _ => unreachable!(),
                };
                *path = prep_steps_bin;
                output
            });

            (output, cmd)
        });

        let register_vmgstool = build.vmgstool.then(|| {
            let output = ctx.reqv(|v| crate::build_vmgstool::Request {
                target: target.clone(),
                profile: CommonProfile::from_release(release),
                with_crypto: true,
                with_test_helpers: true,
                vmgstool: v,
            });
            if copy_extras {
                copy_to_dir.push((
                    extras_dir.to_owned(),
                    output.map(ctx, |x| {
                        Some(match x {
                            crate::build_vmgstool::VmgstoolOutput::WindowsBin { exe: _, pdb } => {
                                pdb
                            }
                            crate::build_vmgstool::VmgstoolOutput::LinuxBin { bin: _, dbg } => dbg,
                        })
                    }),
                ));
            }
            output
        });

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
        ctx.req(
            crate::download_openvmm_vmm_tests_artifacts::Request::CustomCacheDir(
                vmm_test_artifacts_dir,
            ),
        );

        ctx.req(crate::download_openvmm_vmm_tests_artifacts::Request::Download(test_artifacts));
        let test_artifacts_dir =
            ctx.reqv(crate::download_openvmm_vmm_tests_artifacts::Request::GetDownloadFolder);

        ctx.req(crate::install_vmm_tests_deps::Request::Select(deps));
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

        let release_igvm_files =
            ctx.reqv(
                |v| crate::download_release_igvm_files_from_gh::resolve::Request {
                    arch,
                    release_igvm_files: v,
                    release_version:
                        crate::download_release_igvm_files_from_gh::OpenhclReleaseVersion::latest(),
                },
            );

        let extra_env = ctx.reqv(|v| crate::init_vmm_tests_env::Request {
            test_content_dir: ReadVar::from_static(test_content_dir.clone()),
            vmm_tests_target: target_triple.clone(),
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
            disk_images_dir: Some(test_artifacts_dir),
            register_openhcl_igvm_files,
            get_test_log_path: None,
            get_env: v,
            release_igvm_files: Some(release_igvm_files),
            use_relative_paths: build_only,
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

                for cmd in &dep_install_cmds {
                    log::info!("{cmd}");
                }

                if !dep_install_cmds.is_empty() {
                    let script_contents = dep_install_cmds.join("\n");
                    fs_err::write(test_content_dir.join("install_deps.ps1"), script_contents)?;
                }

                Ok(())
            }
        }));

        let nextest_profile = crate::run_cargo_nextest_run::NextestProfile::Default;

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
                .map(|(_, cmd)| ReadVar::from_static(vec![cmd])),
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

        if build_only {
            ctx.emit_side_effect_step(side_effects, [done]);
            if let Some((prep_steps, _)) = register_prep_steps {
                prep_steps.claim_unused(ctx);
            }
        } else {
            side_effects.push(ctx.reqv(crate::install_vmm_tests_deps::Request::Install));
            if let Some((prep_steps, _)) = register_prep_steps {
                side_effects.push(ctx.reqv(|done| crate::run_prep_steps::Request {
                    prep_steps,
                    env: extra_env.clone(),
                    done,
                }));
            }

            let results = ctx.reqv(|v| crate::test_nextest_vmm_tests_archive::Request {
                nextest_archive_file: ReadVar::from_static(NextestVmmTestsArchive {
                    archive_file: nextest_archive_file,
                }),
                nextest_profile,
                nextest_filter_expr: Some(nextest_filter_expr),
                nextest_working_dir: Some(ReadVar::from_static(test_content_dir.clone())),
                nextest_config_file: Some(ReadVar::from_static(nextest_config_file)),
                nextest_bin: Some(ReadVar::from_static(nextest_bin)),
                target: Some(ReadVar::from_static(target_triple)),
                extra_env,
                pre_run_deps: side_effects,
                results: v,
            });

            let junit_xml = results.map(ctx, |r| r.junit_xml);
            let published_results =
                ctx.reqv(|v| flowey_lib_common::publish_test_results::Request {
                    junit_xml,
                    test_label,
                    attachments: BTreeMap::new(), // the logs are already there
                    output_dir: Some(ReadVar::from_static(test_content_dir)),
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
