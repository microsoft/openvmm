// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build an OpenHCL IGVM file using a particular known-good "recipe", which
//! encodes the precise features / build parameters used by each constituent
//! component.
//!
//! By having a clearly enumerated list of recipes, it is possible for multiple
//! pipelines / flows to depend on _precisely_ the same IGVM file, without
//! having to duplicate the non-trivial OpenHCL IGVM build chain.

use crate::build_openhcl_initrd::OpenhclInitrdExtraParams;
use crate::build_openvmm_hcl::MaxTraceLevel;
use crate::build_openvmm_hcl::OpenvmmHclBuildProfile;
use crate::build_openvmm_hcl::OpenvmmHclFeature;
use crate::common::CommonArch;
use crate::common::CommonPlatform;
use crate::common::CommonTriple;
use crate::resolve_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::BuildProfile;
use flowey::node::prelude::*;
use igvmfilegen_config::ResourceType;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// OpenHCL IGVM output
#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum OpenhclIgvmOutput {
    LocalOnlyCustom {
        #[serde(rename = "openhcl-custom.bin")]
        igvm_bin: PathBuf,
        #[serde(rename = "openhcl-tdx.json")]
        igvm_tdx_json: Option<PathBuf>,
        #[serde(rename = "openhcl-snp.json")]
        igvm_snp_json: Option<PathBuf>,
        #[serde(rename = "openhcl-vbs.json")]
        igvm_vbs_json: Option<PathBuf>,
    },
    X64 {
        #[serde(rename = "openhcl-x64.bin")]
        igvm_bin: PathBuf,
    },
    X64Devkern {
        #[serde(rename = "openhcl-x64-devkern.bin")]
        igvm_bin: PathBuf,
    },
    X64TestLinuxDirect {
        #[serde(rename = "openhcl-x64-test-linux-direct.bin")]
        igvm_bin: PathBuf,
    },
    X64TestLinuxDirectDevkern {
        #[serde(rename = "openhcl-x64-test-linux-direct-devkern.bin")]
        igvm_bin: PathBuf,
    },
    X64Cvm {
        #[serde(rename = "openhcl-cvm.bin")]
        igvm_bin: PathBuf,
        #[serde(rename = "openhcl-tdx.json")]
        igvm_tdx_json: PathBuf,
        #[serde(rename = "openhcl-snp.json")]
        igvm_snp_json: PathBuf,
        #[serde(rename = "openhcl-vbs.json")]
        igvm_vbs_json: PathBuf,
    },
    X64CvmDevkern {
        #[serde(rename = "openhcl-x64-cvm-devkern.bin")]
        igvm_bin: PathBuf,
        #[serde(rename = "openhcl-tdx.json")]
        igvm_tdx_json: PathBuf,
        #[serde(rename = "openhcl-snp.json")]
        igvm_snp_json: PathBuf,
        #[serde(rename = "openhcl-vbs.json")]
        igvm_vbs_json: PathBuf,
    },
    Aarch64 {
        #[serde(rename = "openhcl-x64-aarch64.bin")]
        igvm_bin: PathBuf,
    },
    Aarch64Devkern {
        #[serde(rename = "openhcl-x64-aarch64-devkern.bin")]
        igvm_bin: PathBuf,
    },
}

impl Artifact for OpenhclIgvmOutput {}

impl OpenhclIgvmOutput {
    pub fn igvm_bin(&self) -> &Path {
        match self {
            OpenhclIgvmOutput::LocalOnlyCustom { igvm_bin, .. }
            | OpenhclIgvmOutput::X64 { igvm_bin }
            | OpenhclIgvmOutput::X64Devkern { igvm_bin }
            | OpenhclIgvmOutput::X64TestLinuxDirect { igvm_bin }
            | OpenhclIgvmOutput::X64TestLinuxDirectDevkern { igvm_bin }
            | OpenhclIgvmOutput::X64Cvm { igvm_bin, .. }
            | OpenhclIgvmOutput::X64CvmDevkern { igvm_bin, .. }
            | OpenhclIgvmOutput::Aarch64 { igvm_bin }
            | OpenhclIgvmOutput::Aarch64Devkern { igvm_bin } => igvm_bin,
        }
    }

    pub fn recipe(&self) -> Option<OpenhclIgvmRecipe> {
        match self {
            OpenhclIgvmOutput::LocalOnlyCustom { .. } => None,
            OpenhclIgvmOutput::X64 { .. } => Some(OpenhclIgvmRecipe::X64),
            OpenhclIgvmOutput::X64Devkern { .. } => Some(OpenhclIgvmRecipe::X64Devkern),
            OpenhclIgvmOutput::X64TestLinuxDirect { .. } => {
                Some(OpenhclIgvmRecipe::X64TestLinuxDirect)
            }
            OpenhclIgvmOutput::X64TestLinuxDirectDevkern { .. } => {
                Some(OpenhclIgvmRecipe::X64TestLinuxDirectDevkern)
            }
            OpenhclIgvmOutput::X64Cvm { .. } => Some(OpenhclIgvmRecipe::X64Cvm),
            OpenhclIgvmOutput::X64CvmDevkern { .. } => Some(OpenhclIgvmRecipe::X64CvmDevkern),
            OpenhclIgvmOutput::Aarch64 { .. } => Some(OpenhclIgvmRecipe::Aarch64),
            OpenhclIgvmOutput::Aarch64Devkern { .. } => Some(OpenhclIgvmRecipe::Aarch64Devkern),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum OpenhclKernelPackage {
    /// Kernel from the hcl-main branch
    Main,
    /// CVM kernel from the hcl-main branch
    Cvm,
    /// Kernel from the hcl-dev branch
    Dev,
    /// CVM kernel from the hcl-dev brnach
    CvmDev,
}

/// Vtl0 kernel type
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Vtl0KernelType {
    Example,
    LocalOnlyCustom(PathBuf),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum IgvmManifestPath {
    /// Name of an in-tree manifest (located under `vm/loader/manifests`)
    InTree(String),
    /// An absolute path to a custom manifest (for local use only)
    LocalOnlyCustom(PathBuf),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OpenhclIgvmRecipeDetails {
    pub local_only: Option<OpenhclIgvmRecipeDetailsLocalOnly>,

    pub igvm_manifest: IgvmManifestPath,
    pub openhcl_kernel_package: OpenhclKernelPackage,
    pub openvmm_hcl_features: BTreeSet<OpenvmmHclFeature>,
    pub target: CommonTriple,
    pub vtl0_kernel_type: Option<Vtl0KernelType>,
    pub with_uefi: bool,
    pub with_interactive: bool,
    pub with_sidecar: bool,
    pub max_trace_level: MaxTraceLevel,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OpenhclIgvmRecipeDetailsLocalOnly {
    pub openvmm_hcl_no_strip: bool,
    pub openhcl_initrd_extra_params: Option<OpenhclInitrdExtraParams>,
    pub custom_openvmm_hcl: Option<PathBuf>,
    pub custom_openhcl_boot: Option<PathBuf>,
    pub custom_kernel: Option<PathBuf>,
    pub custom_sidecar: Option<PathBuf>,
    pub custom_extra_rootfs: Vec<PathBuf>,
}

#[expect(clippy::large_enum_variant)]
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum OpenhclIgvmRecipeType {
    LocalOnlyCustom(OpenhclIgvmRecipeDetails),
    WellKnown(OpenhclIgvmRecipe),
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenhclIgvmRecipe {
    X64,
    X64Devkern,
    X64TestLinuxDirect,
    X64TestLinuxDirectDevkern,
    X64Cvm,
    X64CvmDevkern,
    Aarch64,
    Aarch64Devkern,
}

impl ArtifactType for OpenhclIgvmRecipe {
    fn name(&self, additional_tag: Option<&str>) -> String {
        let arch_tag = self.arch();
        let tag =
            additional_tag.map_or_else(|| arch_tag.to_string(), |x| format!("{arch_tag}-{x}"));
        format!("{}-openhcl-igvm{}", tag, self.flavor())
    }
}

impl OpenhclIgvmRecipe {
    pub fn non_production_tag(&self) -> String {
        format!("{}{}", self.arch(), self.flavor())
    }

    pub fn non_production_name(&self) -> String {
        format!("openhcl-{}", self.non_production_tag())
    }

    fn flavor(&self) -> &'static str {
        match self {
            OpenhclIgvmRecipe::X64 | OpenhclIgvmRecipe::Aarch64 => "",
            OpenhclIgvmRecipe::X64Devkern | OpenhclIgvmRecipe::Aarch64Devkern => "-devkern",
            OpenhclIgvmRecipe::X64TestLinuxDirect => "-test-linux-direct",
            OpenhclIgvmRecipe::X64TestLinuxDirectDevkern => "-test-linux-direct-devkern",
            OpenhclIgvmRecipe::X64Cvm => "-cvm",
            OpenhclIgvmRecipe::X64CvmDevkern => "-cvm-devkern",
        }
    }

    fn arch(&self) -> &'static str {
        match self {
            OpenhclIgvmRecipe::X64
            | OpenhclIgvmRecipe::X64Devkern
            | OpenhclIgvmRecipe::X64TestLinuxDirect
            | OpenhclIgvmRecipe::X64TestLinuxDirectDevkern
            | OpenhclIgvmRecipe::X64Cvm
            | OpenhclIgvmRecipe::X64CvmDevkern => "x64",
            OpenhclIgvmRecipe::Aarch64 | OpenhclIgvmRecipe::Aarch64Devkern => "aarch64",
        }
    }
}

impl OpenhclIgvmRecipeType {
    pub fn recipe_details(&self, release_cfg: bool) -> OpenhclIgvmRecipeDetails {
        match self {
            Self::LocalOnlyCustom(details) => details.clone(),
            Self::WellKnown(recipe) => recipe.recipe_details(release_cfg),
        }
    }
}

impl OpenhclIgvmRecipe {
    pub fn recipe_details(&self, release_cfg: bool) -> OpenhclIgvmRecipeDetails {
        let base_openvmm_hcl_features = || {
            let mut m = BTreeSet::new();

            m.insert(OpenvmmHclFeature::Tpm);

            if !release_cfg {
                m.insert(OpenvmmHclFeature::Gdb);
            }

            m
        };

        let in_repo_template = |debug_manifest: &'static str, release_manifest: &'static str| {
            IgvmManifestPath::InTree(if release_cfg {
                release_manifest.into()
            } else {
                debug_manifest.into()
            })
        };

        // Debug configurations include --interactive by default, for busybox, gdbserver, and perf.
        let with_interactive = !release_cfg;

        // Save memory and cycles in hot paths by limiting the trace level in
        // release builds.
        let max_trace_level = if release_cfg {
            MaxTraceLevel::Debug
        } else {
            MaxTraceLevel::Trace
        };

        match self {
            Self::X64 => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template("openhcl-x64-dev.json", "openhcl-x64-release.json"),
                openhcl_kernel_package: OpenhclKernelPackage::Main,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::X86_64_LINUX_MUSL,
                vtl0_kernel_type: None,
                with_uefi: true,
                with_interactive,
                with_sidecar: true,
                max_trace_level,
            },
            Self::X64Devkern => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template("openhcl-x64-dev.json", "openhcl-x64-release.json"),
                openhcl_kernel_package: OpenhclKernelPackage::Dev,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::X86_64_LINUX_MUSL,
                vtl0_kernel_type: None,
                with_uefi: true,
                with_interactive,
                with_sidecar: true,
                max_trace_level,
            },
            Self::X64CvmDevkern => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template(
                    "openhcl-x64-cvm-dev.json",
                    "openhcl-x64-cvm-release.json",
                ),
                openhcl_kernel_package: OpenhclKernelPackage::CvmDev,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::X86_64_LINUX_MUSL,
                vtl0_kernel_type: None,
                with_uefi: true,
                with_interactive,
                with_sidecar: false,
                max_trace_level,
            },
            Self::X64TestLinuxDirect => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template(
                    "openhcl-x64-direct-dev.json",
                    "openhcl-x64-direct-release.json",
                ),
                openhcl_kernel_package: OpenhclKernelPackage::Main,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::X86_64_LINUX_MUSL,
                vtl0_kernel_type: Some(Vtl0KernelType::Example),
                with_uefi: false,
                with_interactive,
                with_sidecar: true,
                max_trace_level,
            },
            Self::X64TestLinuxDirectDevkern => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template(
                    "openhcl-x64-direct-dev.json",
                    "openhcl-x64-direct-release.json",
                ),
                openhcl_kernel_package: OpenhclKernelPackage::Dev,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::X86_64_LINUX_MUSL,
                vtl0_kernel_type: Some(Vtl0KernelType::Example),
                with_uefi: false,
                with_interactive,
                with_sidecar: true,
                max_trace_level,
            },
            Self::X64Cvm => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template(
                    "openhcl-x64-cvm-dev.json",
                    "openhcl-x64-cvm-release.json",
                ),
                openhcl_kernel_package: OpenhclKernelPackage::Cvm,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::X86_64_LINUX_MUSL,
                vtl0_kernel_type: None,
                with_uefi: true,
                with_interactive,
                with_sidecar: false,
                max_trace_level,
            },
            Self::Aarch64 => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template(
                    "openhcl-aarch64-dev.json",
                    "openhcl-aarch64-release.json",
                ),
                openhcl_kernel_package: OpenhclKernelPackage::Main,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::AARCH64_LINUX_MUSL,
                vtl0_kernel_type: None,
                with_uefi: true,
                with_interactive: false, // #1234
                with_sidecar: false,
                max_trace_level,
            },
            Self::Aarch64Devkern => OpenhclIgvmRecipeDetails {
                local_only: None,
                igvm_manifest: in_repo_template(
                    "openhcl-aarch64-dev.json",
                    "openhcl-aarch64-release.json",
                ),
                openhcl_kernel_package: OpenhclKernelPackage::Dev,
                openvmm_hcl_features: base_openvmm_hcl_features(),
                target: CommonTriple::AARCH64_LINUX_MUSL,
                vtl0_kernel_type: None,
                with_uefi: true,
                with_interactive: false, // #1234
                with_sidecar: false,
                max_trace_level,
            },
        }
    }
}

flowey_request! {
    pub struct Request {
        pub build_profile: OpenvmmHclBuildProfile,
        pub release_cfg: bool,
        pub recipe: OpenhclIgvmRecipeType,
        pub custom_target: Option<CommonTriple>,
        /// Additional features to enable on top of the recipe's defaults.
        pub extra_features: BTreeSet<OpenvmmHclFeature>,
        pub disable_secure_avic: bool,

        pub built_openvmm_hcl: WriteVar<crate::build_openvmm_hcl::OpenvmmHclOutput>,
        pub built_openhcl_boot: WriteVar<crate::build_openhcl_boot::OpenhclBootOutput>,
        pub built_openhcl_igvm: WriteVar<crate::run_igvmfilegen::IgvmOutput>,
        pub built_sidecar: WriteVar<Option<crate::build_sidecar::SidecarOutput>>,

        pub openhcl_igvm: WriteVar<OpenhclIgvmOutput>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_igvmfilegen::Node>();
        ctx.import::<crate::build_openhcl_boot::Node>();
        ctx.import::<crate::build_openhcl_initrd::Node>();
        ctx.import::<crate::build_openvmm_hcl::Node>();
        ctx.import::<crate::build_sidecar::Node>();
        ctx.import::<crate::resolve_openhcl_kernel_package::Node>();
        ctx.import::<crate::resolve_openvmm_deps::Node>();
        ctx.import::<crate::resolve_openvmm_test_initrd::Node>();
        ctx.import::<crate::resolve_openvmm_test_linux_kernel::Node>();
        ctx.import::<crate::download_uefi_mu_msvm::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::run_igvmfilegen::Node>();
        ctx.import::<crate::run_split_debug_info::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            build_profile,
            release_cfg,
            recipe,
            custom_target,
            extra_features,
            disable_secure_avic,
            built_openvmm_hcl,
            built_openhcl_boot,
            built_openhcl_igvm,
            built_sidecar,
            openhcl_igvm,
        } = request;

        let OpenhclIgvmRecipeDetails {
            local_only,
            igvm_manifest,
            openhcl_kernel_package,
            mut openvmm_hcl_features,
            target,
            vtl0_kernel_type,
            with_uefi,
            with_interactive,
            with_sidecar,
            max_trace_level,
        } = recipe.recipe_details(release_cfg);

        openvmm_hcl_features.extend(extra_features);

        if disable_secure_avic {
            openvmm_hcl_features.insert(OpenvmmHclFeature::LocalOnlyCustom(
                "disable_secure_avic".into(),
            ));
        }

        let OpenhclIgvmRecipeDetailsLocalOnly {
            openvmm_hcl_no_strip,
            openhcl_initrd_extra_params,
            custom_openvmm_hcl,
            custom_openhcl_boot,
            custom_kernel,
            custom_sidecar,
            custom_extra_rootfs,
        } = local_only.unwrap_or(OpenhclIgvmRecipeDetailsLocalOnly {
            openvmm_hcl_no_strip: false,
            openhcl_initrd_extra_params: None,
            custom_openvmm_hcl: None,
            custom_openhcl_boot: None,
            custom_kernel: None,
            custom_sidecar: None,
            custom_extra_rootfs: Vec::new(),
        });

        let target = custom_target.unwrap_or(target);
        let arch = CommonArch::from_triple(&target.as_triple())
            .with_context(|| format!("cannot build openHCL from recipe on {target}"))?;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let kernel_kind = match openhcl_kernel_package {
            OpenhclKernelPackage::Main => OpenhclKernelPackageKind::Main,
            OpenhclKernelPackage::Cvm => OpenhclKernelPackageKind::Cvm,
            OpenhclKernelPackage::Dev => OpenhclKernelPackageKind::Dev,
            OpenhclKernelPackage::CvmDev => OpenhclKernelPackageKind::CvmDev,
        };

        // Get the kernel package root for initrd building (needs metadata)
        let vtl2_kernel_package_root =
            ctx.reqv(
                |v| crate::resolve_openhcl_kernel_package::Request::GetPackageRoot {
                    kind: kernel_kind,
                    arch,
                    pkg: v,
                },
            );

        // Get the modules path from the resolve node
        let vtl2_kernel_modules =
            ctx.reqv(
                |v| crate::resolve_openhcl_kernel_package::Request::GetModules {
                    kind: kernel_kind,
                    arch,
                    modules: v,
                },
            );

        // Get the kernel metadata path from the resolve node
        let vtl2_kernel_metadata =
            ctx.reqv(
                |v| crate::resolve_openhcl_kernel_package::Request::GetMetadata {
                    kind: kernel_kind,
                    arch,
                    metadata: v,
                },
            );

        let uefi_resource: Option<UefiResource> = with_uefi.then(|| UefiResource {
            msvm_fd: ctx
                .reqv(|v| crate::download_uefi_mu_msvm::Request::GetMsvmFd { arch, msvm_fd: v }),
        });

        let vtl0_kernel_resource = vtl0_kernel_type.map(|typ| {
            let kernel = if let Vtl0KernelType::LocalOnlyCustom(path) = typ {
                ReadVar::from_static(path)
            } else {
                match typ {
                    Vtl0KernelType::Example => ctx.reqv(|v| {
                        crate::resolve_openvmm_test_linux_kernel::Request::Get(
                            crate::resolve_openvmm_test_linux_kernel::OpenvmmTestKernelFile::Kernel,
                            arch,
                            crate::resolve_openvmm_test_linux_kernel::DEFAULT_LINUX_TEST_KERNEL_VERSION,
                            v,
                        )
                    }),
                    Vtl0KernelType::LocalOnlyCustom(_) => unreachable!("special cased above"),
                }
            };

            let initrd = ctx.reqv(|v| {
                crate::resolve_openvmm_test_initrd::Request::Get(arch, v)
            });

            Vtl0KernelResource { kernel, initrd }
        });

        // build sidecar
        let sidecar_bin = if with_sidecar {
            let sidecar_bin = if let Some(path) = custom_sidecar {
                ctx.emit_rust_stepv("set custom_sidecar", |_ctx| {
                    |_rt| {
                        let fake_dbg_path = std::env::current_dir()?
                            .join("fake_sidecar.dbg")
                            .absolute()?;
                        fs_err::write(&fake_dbg_path, "")?;

                        Ok(crate::build_sidecar::SidecarOutput {
                            bin: path,
                            dbg: fake_dbg_path,
                        })
                    }
                })
            } else {
                ctx.reqv(|v| crate::build_sidecar::Request {
                    build_params: crate::build_sidecar::SidecarBuildParams {
                        arch,
                        profile: match build_profile {
                            OpenvmmHclBuildProfile::Debug => {
                                crate::build_sidecar::SidecarBuildProfile::Debug
                            }
                            OpenvmmHclBuildProfile::Release
                            | OpenvmmHclBuildProfile::OpenvmmHclShip => {
                                crate::build_sidecar::SidecarBuildProfile::Release
                            }
                        },
                    },
                    sidecar: v,
                })
            };
            sidecar_bin.write_into_with(ctx, built_sidecar, Some);
            Some(sidecar_bin)
        } else {
            built_sidecar.write_static(ctx, None);
            None
        };

        // build openvmm_hcl bin
        let openvmm_hcl_bin = if let Some(ref path) = custom_openvmm_hcl {
            let path = path.clone();
            ctx.emit_rust_stepv("set custom_openvmm_hcl", |_ctx| {
                |_rt| {
                    Ok(crate::build_openvmm_hcl::OpenvmmHclOutput {
                        bin: path,
                        dbg: None,
                    })
                }
            })
        } else {
            ctx.reqv(|v| {
                crate::build_openvmm_hcl::Request {
                    build_params: crate::build_openvmm_hcl::OpenvmmHclBuildParams {
                        target: target.clone(),
                        profile: build_profile,
                        features: openvmm_hcl_features,
                        // manually strip later, depending on provided igvm flags
                        no_split_dbg_info: true,
                        max_trace_level,
                    },
                    openvmm_hcl_output: v,
                }
            })
        };

        // build igvmfilegen (always built for host arch)
        let igvmfilegen_arch: CommonArch = ctx.arch().try_into()?;

        let igvmfilegen = ctx.reqv(|v| crate::build_igvmfilegen::Request {
            build_params: crate::build_igvmfilegen::IgvmfilegenBuildParams {
                target: CommonTriple::Common {
                    arch: igvmfilegen_arch,
                    platform: CommonPlatform::LinuxGnu,
                },
                profile: BuildProfile::Light,
            },
            igvmfilegen: v,
        });

        // build openhcl_boot
        let openhcl_boot_bin = if let Some(path) = custom_openhcl_boot {
            ctx.emit_rust_stepv("set custom_openhcl_boot", |_ctx| {
                |_rt| {
                    let fake_dbg_path = std::env::current_dir()?.join("fake.dbg").absolute()?;
                    fs_err::write(&fake_dbg_path, "")?;

                    Ok(crate::build_openhcl_boot::OpenhclBootOutput {
                        bin: path,
                        dbg: fake_dbg_path,
                    })
                }
            })
        } else {
            ctx.reqv(|v| crate::build_openhcl_boot::Request {
                build_params: crate::build_openhcl_boot::OpenhclBootBuildParams {
                    arch,
                    profile: match build_profile {
                        OpenvmmHclBuildProfile::Debug => {
                            crate::build_openhcl_boot::OpenhclBootBuildProfile::Debug
                        }
                        OpenvmmHclBuildProfile::Release
                        | OpenvmmHclBuildProfile::OpenvmmHclShip => {
                            crate::build_openhcl_boot::OpenhclBootBuildProfile::Release
                        }
                    },
                },
                openhcl_boot: v,
            })
        };
        openhcl_boot_bin.write_into(ctx, built_openhcl_boot);

        let use_stripped_openvmm_hcl = {
            if custom_openvmm_hcl.is_some() {
                // trust the user knows what they are doing if they specified a
                // custom bin
                false
            } else {
                !openvmm_hcl_no_strip
            }
        };

        // use the stripped or unstripped openvmm_hcl as requested
        let openvmm_hcl_bin = if use_stripped_openvmm_hcl {
            let (read, write) = ctx.new_var();
            let (read_dbg, write_dbg) = ctx.new_var();

            let in_bin = openvmm_hcl_bin.map(ctx, |o| o.bin);
            ctx.req(crate::run_split_debug_info::Request {
                arch,
                in_bin,
                out_bin: write,
                out_dbg_info: write_dbg,
                reproducible_without_debuglink: matches!(
                    ctx.platform(),
                    FlowPlatform::Linux(FlowPlatformLinuxDistro::Nix)
                ),
            });

            read.zip(ctx, read_dbg).map(ctx, |(bin, dbg)| {
                crate::build_openvmm_hcl::OpenvmmHclOutput {
                    bin,
                    dbg: Some(dbg),
                }
            })
        } else {
            openvmm_hcl_bin
        };

        // report the built openvmm_hcl
        openvmm_hcl_bin.write_into(ctx, built_openvmm_hcl);

        let initrd = {
            let rootfs_config = [openvmm_repo_path.map(ctx, |p| p.join("openhcl/rootfs.config"))]
                .into_iter()
                .chain(
                    custom_extra_rootfs
                        .into_iter()
                        .map(|p| ReadVar::from_static(p)),
                )
                .collect();
            let openvmm_hcl_bin = openvmm_hcl_bin.map(ctx, |o| o.bin);

            ctx.reqv(|v| crate::build_openhcl_initrd::Request {
                interactive: with_interactive,
                arch,
                extra_params: openhcl_initrd_extra_params,
                rootfs_config,
                extra_env: None,
                kernel_package_root: vtl2_kernel_package_root.clone(),
                kernel_modules: vtl2_kernel_modules,
                kernel_metadata: vtl2_kernel_metadata,
                bin_openhcl: openvmm_hcl_bin,
                initrd: v,
            })
        };

        let kernel = if let Some(path) = custom_kernel {
            ReadVar::from_static(path)
        } else {
            ctx.reqv(
                |v| crate::resolve_openhcl_kernel_package::Request::GetKernel {
                    kind: kernel_kind,
                    arch,
                    kernel: v,
                },
            )
        };

        let resources = ctx.emit_minor_rust_stepv("enumerate igvm resources", |ctx| {
            let initrd = initrd.claim(ctx);
            let kernel = kernel.claim(ctx);
            let openhcl_boot_bin = openhcl_boot_bin.claim(ctx);
            let sidecar_bin = sidecar_bin.claim(ctx);
            let uefi_resource = uefi_resource.claim(ctx);
            let vtl0_kernel_resource = vtl0_kernel_resource.claim(ctx);
            |rt| {
                let mut resources = BTreeMap::<ResourceType, PathBuf>::new();
                resources.insert(ResourceType::UnderhillKernel, rt.read(kernel));
                resources.insert(ResourceType::UnderhillInitrd, rt.read(initrd).initrd);
                resources.insert(ResourceType::OpenhclBoot, rt.read(openhcl_boot_bin).bin);
                if let Some(sidecar_bin) = sidecar_bin {
                    resources.insert(ResourceType::UnderhillSidecar, rt.read(sidecar_bin).bin);
                }
                if let Some(uefi_resource) = uefi_resource {
                    uefi_resource.add_to_resources(&mut resources, rt);
                }
                if let Some(vtl0_kernel_resource) = vtl0_kernel_resource {
                    vtl0_kernel_resource.add_to_resources(&mut resources, rt);
                }
                resources
            }
        });

        let igvmfilegen = igvmfilegen.map(ctx, |o| match o {
            crate::build_igvmfilegen::IgvmfilegenOutput::LinuxBin { bin, dbg: _ } => bin,
            crate::build_igvmfilegen::IgvmfilegenOutput::WindowsBin { exe, pdb: _ } => exe,
        });

        let manifest = match igvm_manifest {
            IgvmManifestPath::InTree(path) => {
                openvmm_repo_path.map(ctx, |p| p.join("vm/loader/manifests").join(path))
            }
            IgvmManifestPath::LocalOnlyCustom(p) => ReadVar::from_static(p),
        };

        let igvm = ctx.reqv(|v| crate::run_igvmfilegen::Request {
            igvmfilegen,
            manifest,
            resources,
            disable_secure_avic,
            igvm: v,
        });

        igvm.write_into(ctx, built_openhcl_igvm);
        igvm.write_into_with(ctx, openhcl_igvm, move |x| match recipe {
            OpenhclIgvmRecipeType::LocalOnlyCustom(_) => OpenhclIgvmOutput::LocalOnlyCustom {
                igvm_bin: x.igvm_bin,
                igvm_tdx_json: x.igvm_tdx_json,
                igvm_snp_json: x.igvm_snp_json,
                igvm_vbs_json: x.igvm_vbs_json,
            },
            OpenhclIgvmRecipeType::WellKnown(recipe) => match recipe {
                OpenhclIgvmRecipe::X64 => OpenhclIgvmOutput::X64 {
                    igvm_bin: x.igvm_bin,
                },
                OpenhclIgvmRecipe::X64Devkern => OpenhclIgvmOutput::X64Devkern {
                    igvm_bin: x.igvm_bin,
                },
                OpenhclIgvmRecipe::X64TestLinuxDirect => OpenhclIgvmOutput::X64TestLinuxDirect {
                    igvm_bin: x.igvm_bin,
                },
                OpenhclIgvmRecipe::X64TestLinuxDirectDevkern => {
                    OpenhclIgvmOutput::X64TestLinuxDirectDevkern {
                        igvm_bin: x.igvm_bin,
                    }
                }
                OpenhclIgvmRecipe::X64Cvm => OpenhclIgvmOutput::X64Cvm {
                    igvm_bin: x.igvm_bin,
                    igvm_tdx_json: x.igvm_tdx_json.unwrap(),
                    igvm_snp_json: x.igvm_snp_json.unwrap(),
                    igvm_vbs_json: x.igvm_vbs_json.unwrap(),
                },
                OpenhclIgvmRecipe::X64CvmDevkern => OpenhclIgvmOutput::X64CvmDevkern {
                    igvm_bin: x.igvm_bin,
                    igvm_tdx_json: x.igvm_tdx_json.unwrap(),
                    igvm_snp_json: x.igvm_snp_json.unwrap(),
                    igvm_vbs_json: x.igvm_vbs_json.unwrap(),
                },
                OpenhclIgvmRecipe::Aarch64 => OpenhclIgvmOutput::Aarch64 {
                    igvm_bin: x.igvm_bin,
                },
                OpenhclIgvmRecipe::Aarch64Devkern => OpenhclIgvmOutput::Aarch64Devkern {
                    igvm_bin: x.igvm_bin,
                },
            },
        });

        Ok(())
    }
}

#[derive(Debug)]
pub struct UefiResource<C = VarNotClaimed> {
    pub msvm_fd: ReadVar<PathBuf, C>,
}

impl ClaimVar for UefiResource {
    type Claimed = UefiResource<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> UefiResource<VarClaimed> {
        UefiResource {
            msvm_fd: self.msvm_fd.claim(ctx),
        }
    }
}

impl UefiResource<VarClaimed> {
    pub fn add_to_resources(
        self,
        resources: &mut BTreeMap<ResourceType, PathBuf>,
        rt: &mut RustRuntimeServices<'_>,
    ) {
        let path = rt.read(self.msvm_fd);
        resources.insert(ResourceType::Uefi, path);
    }
}

pub struct Vtl0KernelResource<C = VarNotClaimed> {
    pub kernel: ReadVar<PathBuf, C>,
    pub initrd: ReadVar<PathBuf, C>,
}

impl ClaimVar for Vtl0KernelResource {
    type Claimed = Vtl0KernelResource<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Vtl0KernelResource<VarClaimed> {
        Vtl0KernelResource {
            kernel: self.kernel.claim(ctx),
            initrd: self.initrd.claim(ctx),
        }
    }
}

impl Vtl0KernelResource<VarClaimed> {
    pub fn add_to_resources(
        self,
        resources: &mut BTreeMap<ResourceType, PathBuf>,
        rt: &mut RustRuntimeServices<'_>,
    ) {
        let kernel = rt.read(self.kernel);
        let initrd = rt.read(self.initrd);
        resources.insert(ResourceType::LinuxKernel, kernel);
        resources.insert(ResourceType::LinuxInitrd, initrd);
    }
}
