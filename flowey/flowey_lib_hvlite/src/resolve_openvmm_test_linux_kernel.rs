// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download files from a Linux test kernel `openvmm-deps` GitHub release
//! artifact, or use a local path if specified.
//!
//! Each [`LinuxTestKernelVersion`] variant corresponds to its own
//! per-kernel-version GitHub release artifact (e.g.
//! `openvmm-test-linux-6.1.<arch>.<ver>.tar.gz`), so consumers can target
//! different kernel versions independently. Each archive contains the
//! primary kernel image (`vmlinux` on x86_64, `Image` on aarch64) and, on
//! x86_64, an additional `bzImage`-format kernel — see
//! [`OpenvmmTestKernelFile`] to select between them. The matching guest-
//! userland initrd is shared across kernel versions and lives in its own
//! node (see [`crate::resolve_openvmm_test_initrd`]).

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Which Linux test kernel version to fetch from the openvmm-deps GitHub
/// release.
#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinuxTestKernelVersion {
    Linux6_1,
    Linux6_18,
    /// The `kvm-cca-dev` kernel (Linux 7.1.0-rc1 at time of writing).
    ///
    /// Published **aarch64-only**. Beyond the ARM CCA host bits it is built
    /// for, it also enables the P2PDMA / vfio-dmabuf / iommufd config
    /// (`CONFIG_PCI_P2PDMA`, `CONFIG_VFIO_PCI_DMABUF`, `CONFIG_IOMMUFD`,
    /// `CONFIG_ARM_SMMU_V3`) that the incubator's VFIO device-assignment tests
    /// need to exercise device-BAR P2P DMA, which predate the 6.18 test kernel.
    KvmCcaDev,
}

impl LinuxTestKernelVersion {
    /// The version string used in the openvmm-deps GitHub release artifact
    /// filename (e.g. `"6.1"` for `openvmm-test-linux-6.1.<arch>.<ver>.tar.gz`).
    pub fn artifact_tag(self) -> &'static str {
        match self {
            Self::Linux6_1 => "6.1",
            Self::Linux6_18 => "6.18",
            Self::KvmCcaDev => "kvm-cca-dev",
        }
    }

    /// Whether this kernel version is published for the given architecture.
    ///
    /// Most versions ship for both architectures; `kvm-cca-dev` is aarch64-only.
    pub fn is_available_for(self, arch: CommonArch) -> bool {
        match self {
            Self::Linux6_1 | Self::Linux6_18 => true,
            Self::KvmCcaDev => matches!(arch, CommonArch::Aarch64),
        }
    }
}

/// Which file to extract from a per-(arch, kver) `openvmm-test-linux` archive.
#[derive(Serialize, Deserialize, Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpenvmmTestKernelFile {
    /// Primary kernel image: `vmlinux` on x86_64, `Image` on aarch64.
    Kernel,
    /// `bzImage`-format kernel image. Only available on x86_64.
    BzImage,
}

impl OpenvmmTestKernelFile {
    /// Whether this file is shipped in the archive for the given architecture.
    pub fn is_available_for(self, arch: CommonArch) -> bool {
        match self {
            Self::Kernel => true,
            Self::BzImage => matches!(arch, CommonArch::X86_64),
        }
    }

    /// The filename of this file inside the per-(arch, kver) archive.
    pub fn filename(self, arch: CommonArch) -> &'static str {
        match self {
            Self::Kernel => match arch {
                CommonArch::X86_64 => "vmlinux",
                CommonArch::Aarch64 => "Image",
            },
            Self::BzImage => "bzImage",
        }
    }
}

/// The default Linux test kernel version. Call sites that don't otherwise care
/// which kernel they're using should pass this.
pub const DEFAULT_LINUX_TEST_KERNEL_VERSION: LinuxTestKernelVersion =
    LinuxTestKernelVersion::Linux6_18;

/// The Linux test kernel used as the **L1 host image** for the aarch64 QEMU-TCG
/// incubator. Unlike [`DEFAULT_LINUX_TEST_KERNEL_VERSION`], this must ship the
/// P2PDMA / vfio-dmabuf / iommufd config
/// ([`LinuxTestKernelVersion::KvmCcaDev`], Linux 7.1.0-rc1) so incubator VFIO
/// device-assignment tests can exercise device-BAR peer-to-peer DMA. Aarch64
/// only (the incubator is aarch64-only).
pub const INCUBATOR_LINUX_TEST_KERNEL_VERSION: LinuxTestKernelVersion =
    LinuxTestKernelVersion::KvmCcaDev;

flowey_config! {
    /// Config for the resolve_openvmm_test_linux_kernel node.
    pub struct Config {
        /// Specify version of the github release to pull from
        pub version: Option<String>,
        /// Use locally downloaded openvmm-test-linux contents, keyed by
        /// (architecture, kernel version)
        pub local_paths: BTreeMap<(CommonArch, LinuxTestKernelVersion), ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get the path to a specific file from the per-(arch, kver) archive.
        Get(
            OpenvmmTestKernelFile,
            CommonArch,
            LinuxTestKernelVersion,
            WriteVar<PathBuf>,
        ),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let Config {
            version,
            local_paths,
        } = config;
        let mut deps: BTreeMap<
            (OpenvmmTestKernelFile, CommonArch, LinuxTestKernelVersion),
            Vec<WriteVar<PathBuf>>,
        > = BTreeMap::new();

        for req in requests {
            match req {
                Request::Get(file, arch, kver, var) => {
                    if !kver.is_available_for(arch) {
                        anyhow::bail!(
                            "test kernel {:?} is not published for {arch:?}",
                            kver.artifact_tag()
                        );
                    }
                    if !file.is_available_for(arch) {
                        anyhow::bail!(
                            "{file:?} is not available in the openvmm-test-linux archive for {arch:?}"
                        );
                    }
                    deps.entry((file, arch, kver)).or_default().push(var);
                }
            }
        }

        if version.is_some() && !local_paths.is_empty() {
            anyhow::bail!("Cannot specify both Version and LocalPath requests");
        }

        if version.is_none() && local_paths.is_empty() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if deps.is_empty() {
            return Ok(());
        }

        if !local_paths.is_empty() {
            ctx.emit_rust_step("use local openvmm-test-linux", |ctx| {
                let deps = deps.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(key, var)| (key, var.claim(ctx)))
                    .collect();
                move |rt| {
                    let resolved_paths: BTreeMap<(CommonArch, LinuxTestKernelVersion), PathBuf> =
                        local_paths
                            .into_iter()
                            .map(|(key, var)| (key, rt.read(var)))
                            .collect();

                    for ((file, arch, kver), vars) in deps {
                        let base_dir = resolved_paths.get(&(arch, kver)).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for ({:?}, {:?})", arch, kver)
                        })?;
                        let path = base_dir.join(file.filename(arch));
                        rt.write_all(vars, &path)
                    }

                    Ok(())
                }
            });

            return Ok(());
        }

        // The same per-(arch, kver) archive can satisfy multiple file
        // requests (e.g. `Kernel` and `BzImage` for the same x86_64 6.1
        // archive), so dedupe download + extract on `(arch, kver)`.
        let needed_archives: BTreeSet<(CommonArch, LinuxTestKernelVersion)> =
            deps.keys().map(|(_, arch, kver)| (*arch, *kver)).collect();

        let mut archives = BTreeMap::new();
        for (arch, kver) in needed_archives {
            let version = version.clone().expect("local requests handled above");
            let arch_str = match arch {
                CommonArch::X86_64 => "x86_64",
                CommonArch::Aarch64 => "aarch64",
            };
            let kver_str = kver.artifact_tag();
            let archive = ctx.reqv(|v| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "openvmm-deps".into(),
                needs_auth: false,
                tag: version.clone(),
                file_name: format!("openvmm-test-linux-{kver_str}.{arch_str}.{version}.tar.gz"),
                path: v,
            });
            archives.insert((arch, kver), archive);
        }

        let persistent_dir = ctx.persistent_dir();

        ctx.emit_rust_step("unpack openvmm-test-linux archives", |ctx| {
            let persistent_dir = persistent_dir.claim(ctx);
            let archives = archives.claim(ctx);
            let deps = deps.claim(ctx);
            let version = version.clone().expect("local requests handled above");
            move |rt| {
                let persistent_dir = persistent_dir.map(|d| rt.read(d));

                let mut extract_dirs = BTreeMap::new();
                for (key, archive) in archives {
                    let file = rt.read(archive);
                    let dir = flowey_lib_common::_util::extract::extract_tar_gz_if_new(
                        rt,
                        persistent_dir.as_deref(),
                        &file,
                        &version,
                    )?;
                    extract_dirs.insert(key, dir);
                }

                for ((file, arch, kver), vars) in deps {
                    let path = extract_dirs[&(arch, kver)].join(file.filename(arch));
                    rt.write_all(vars, &path)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
