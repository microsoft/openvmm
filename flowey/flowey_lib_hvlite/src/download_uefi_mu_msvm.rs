// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download pre-built mu_msvm package from its GitHub Release.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

enum FirmwareVersion {
    Pinned(String),
    Resolved(ReadVar<Release>),
}

/// Firmware core and toolchain used by the RELEASE build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FirmwareFlavor {
    LegacyVs2022,
    LegacyClangPdb,
    PatinaClangPdb,
}

impl FirmwareFlavor {
    fn file_name(self, arch: CommonArch) -> anyhow::Result<&'static str> {
        Ok(match (self, arch) {
            (Self::LegacyVs2022, CommonArch::X86_64) => "RELEASE-X64-VS2022-artifacts.tar.gz",
            (Self::LegacyVs2022, CommonArch::Aarch64) => {
                anyhow::bail!("mu_msvm does not support AARCH64 with VS2022")
            }
            (Self::LegacyClangPdb, CommonArch::X86_64) => "firmware-RELEASE-X64-CLANGPDB.tar.gz",
            (Self::LegacyClangPdb, CommonArch::Aarch64) => {
                "RELEASE-AARCH64-CLANGPDB-artifacts.tar.gz"
            }
            (Self::PatinaClangPdb, CommonArch::X86_64) => {
                "firmware-RELEASE-X64-CLANGPDB-patina.tar.gz"
            }
            (Self::PatinaClangPdb, CommonArch::Aarch64) => {
                "firmware-RELEASE-AARCH64-CLANGPDB-patina.tar.gz"
            }
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl Release {
    fn unique_asset(&self, file_name: &str) -> anyhow::Result<&Asset> {
        let mut matches = self.assets.iter().filter(|asset| asset.name == file_name);
        let asset = matches.next().ok_or_else(|| {
            anyhow::anyhow!("missing asset {file_name} in release {}", self.tag_name)
        })?;
        anyhow::ensure!(
            matches.next().is_none(),
            "duplicate asset {file_name} in release {}",
            self.tag_name
        );
        Ok(asset)
    }
}

fn download_firmware(
    ctx: &mut NodeCtx<'_>,
    version: FirmwareVersion,
    flavor: FirmwareFlavor,
    arch: CommonArch,
    out_vars: Vec<WriteVar<PathBuf>>,
) -> anyhow::Result<()> {
    let file_name = flavor.file_name(arch)?;
    let extract_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);
    let (archive, archive_version) = match version {
        FirmwareVersion::Pinned(version) => {
            let archive = ctx.reqv(|path| flowey_lib_common::download_gh_release::Request {
                repo_owner: "microsoft".into(),
                repo_name: "mu_msvm".into(),
                needs_auth: false,
                tag: format!("v{version}"),
                file_name: file_name.into(),
                path,
            });
            (
                archive,
                ReadVar::from_static(format!("{version}-{file_name}")),
            )
        }
        FirmwareVersion::Resolved(release) => {
            let archive = ctx.emit_rust_stepv(format!("download mu_msvm {file_name}"), |ctx| {
                let release = release.claim(ctx);
                move |rt| {
                    let release = rt.read(release);
                    let asset = release.unique_asset(file_name)?;
                    let url = &asset.browser_download_url;
                    let mut cmd = flowey::shell_cmd!(rt, "curl --fail -L {url} -o {file_name}");
                    if matches!(rt.platform(), FlowPlatform::Windows) {
                        cmd = cmd.arg("--ssl-revoke-best-effort");
                    }
                    cmd.run()?;
                    Ok((
                        rt.sh.current_dir().join(file_name),
                        format!("{}-{file_name}", release.tag_name),
                    ))
                }
            });
            (
                archive.clone().map(ctx, |(path, _)| path),
                archive.map(ctx, |(_, version)| version),
            )
        }
    };
    ctx.emit_rust_step(
        format!(
            "unpack mu_msvm package ({})",
            match arch {
                CommonArch::X86_64 => "x64",
                CommonArch::Aarch64 => "aarch64",
            }
        ),
        |ctx| {
            let extract_deps = extract_deps.claim(ctx);
            let archive = archive.claim(ctx);
            let archive_version = archive_version.claim(ctx);
            let out_vars = out_vars.claim(ctx);
            move |rt| {
                let archive = rt.read(archive);
                let version = rt.read(archive_version);
                let extract_dir = flowey_lib_common::_util::extract::extract_zip_if_new(
                    rt,
                    extract_deps,
                    &archive,
                    &version,
                )?;
                let msvm_fd = extract_dir.join("FV/MSVM.fd");
                for out in out_vars {
                    rt.write(out, &msvm_fd);
                }
                Ok(())
            }
        },
    );
    Ok(())
}

flowey_config! {
    /// Config for the download_uefi_mu_msvm node.
    pub struct Config {
        /// Specify version of mu_msvm to use
        pub version: Option<String>,
        /// Firmware core and toolchain to download for each architecture
        pub flavors: BTreeMap<CommonArch, FirmwareFlavor>,
        /// Use a local MSVM.fd path, keyed by architecture
        pub local_paths: BTreeMap<CommonArch, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Download the mu_msvm package for the given arch
        GetMsvmFd {
            arch: CommonArch,
            msvm_fd: WriteVar<PathBuf>
        }
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
        let version = config.version;
        let local_paths = config.local_paths;
        let mut reqs: BTreeMap<CommonArch, Vec<WriteVar<PathBuf>>> = BTreeMap::new();

        for req in requests {
            match req {
                Request::GetMsvmFd { arch, msvm_fd } => reqs.entry(arch).or_default().push(msvm_fd),
            }
        }

        if version.is_some() && !local_paths.is_empty() {
            anyhow::bail!("Cannot specify both Version and LocalPath requests");
        }

        if version.is_none() && local_paths.is_empty() {
            anyhow::bail!("Must specify a Version or LocalPath request");
        }

        // -- end of req processing -- //

        if reqs.is_empty() {
            return Ok(());
        }

        if !local_paths.is_empty() {
            ctx.emit_rust_step("use local mu_msvm UEFI", |ctx| {
                let reqs = reqs.claim(ctx);
                let local_paths: BTreeMap<_, _> = local_paths
                    .into_iter()
                    .map(|(arch, var)| (arch, var.claim(ctx)))
                    .collect();
                move |rt| {
                    for (arch, out_vars) in reqs {
                        let msvm_fd_var = local_paths.get(&arch).ok_or_else(|| {
                            anyhow::anyhow!("No local path specified for architecture {:?}", arch)
                        })?;
                        let msvm_fd = rt.read(msvm_fd_var.clone());
                        for var in out_vars {
                            log::info!(
                                "using local uefi for {} at path {:?}",
                                match arch {
                                    CommonArch::X86_64 => "x64",
                                    CommonArch::Aarch64 => "aarch64",
                                },
                                msvm_fd
                            );
                            rt.write(var, &msvm_fd);
                        }
                    }
                    Ok(())
                }
            });

            return Ok(());
        }

        let version = version.expect("local paths handled above");

        for (arch, out_vars) in reqs {
            let flavor = config.flavors.get(&arch).copied().ok_or_else(|| {
                anyhow::anyhow!("No mu_msvm firmware flavor configured for {arch:?}")
            })?;
            download_firmware(
                ctx,
                FirmwareVersion::Pinned(version.clone()),
                flavor,
                arch,
                out_vars,
            )?;
        }

        Ok(())
    }
}

/// Resolve and share a single latest Patina release across a pipeline's jobs.
pub mod latest_patina {
    use super::FirmwareFlavor;
    use super::FirmwareVersion;
    use super::Release;
    use super::download_firmware;
    use crate::common::CommonArch;
    use flowey::node::prelude::*;

    flowey_request! {
        pub enum Request {
            Download {
                artifact_dir: ReadVar<PathBuf>,
                done: WriteVar<SideEffect>,
            },
            UseFirmware {
                artifact_dir: ReadVar<PathBuf>,
                arch: CommonArch,
            },
        }
    }

    new_simple_flow_node!(struct Node);

    impl SimpleFlowNode for Node {
        type Request = Request;

        fn imports(ctx: &mut ImportCtx<'_>) {
            ctx.import::<flowey_lib_common::use_gh_cli::Node>();
            ctx.import::<flowey_lib_common::download_gh_release::Node>();
            ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
            ctx.import::<crate::_jobs::cfg_versions::Node>();
        }

        fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
            match request {
                Request::UseFirmware { artifact_dir, arch } => {
                    let path = artifact_dir.map(ctx, move |dir| dir.join(firmware_name(arch)));
                    ctx.req(crate::_jobs::cfg_versions::Request::LocalUefi(arch, path));
                }
                Request::Download { artifact_dir, done } => {
                    let gh_cli = ctx.reqv(flowey_lib_common::use_gh_cli::Request::Get);
                    let release = ctx.emit_rust_stepv("resolve latest mu_msvm release", |ctx| {
                        let gh_cli = gh_cli.claim(ctx);
                        let artifact_dir = artifact_dir.clone().claim(ctx);
                        move |rt| {
                            let gh_cli = rt.read(gh_cli);
                            let artifact_dir = rt.read(artifact_dir);
                            let release_json = flowey::shell_cmd!(
                                rt,
                                "{gh_cli} api repos/microsoft/mu_msvm/releases/latest"
                            )
                            .read()?;
                            let release: Release = serde_json::from_str(&release_json)?;
                            let tag = &release.tag_name;
                            log::info!("using mu_msvm Patina release {tag}");
                            fs_err::create_dir_all(&artifact_dir)?;
                            fs_err::write(artifact_dir.join("release.json"), release_json)?;
                            Ok(release)
                        }
                    });
                    let mut firmware = Vec::new();
                    for arch in [CommonArch::X86_64, CommonArch::Aarch64] {
                        let (read, write) = ctx.new_var();
                        download_firmware(
                            ctx,
                            FirmwareVersion::Resolved(release.clone()),
                            FirmwareFlavor::PatinaClangPdb,
                            arch,
                            vec![write],
                        )?;
                        firmware.push((arch, read));
                    }
                    ctx.emit_rust_step("publish Patina firmware", |ctx| {
                        let artifact_dir = artifact_dir.claim(ctx);
                        let firmware = firmware
                            .into_iter()
                            .map(|(arch, path)| (arch, path.claim(ctx)))
                            .collect::<Vec<_>>();
                        done.claim(ctx);
                        move |rt| {
                            let artifact_dir = rt.read(artifact_dir);
                            for (arch, path) in firmware {
                                fs_err::copy(
                                    rt.read(path),
                                    artifact_dir.join(firmware_name(arch)),
                                )?;
                            }
                            Ok(())
                        }
                    });
                }
            }
            Ok(())
        }
    }

    fn firmware_name(arch: CommonArch) -> &'static str {
        match arch {
            CommonArch::X86_64 => "MSVM-X64.fd",
            CommonArch::Aarch64 => "MSVM-AARCH64.fd",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::Asset;
        use super::super::FirmwareFlavor;
        use super::Release;
        use crate::common::CommonArch;
        use test_with_tracing::test;

        #[test]
        fn firmware_asset_names() {
            for (arch, legacy_flavor, legacy, patina) in [
                (
                    CommonArch::X86_64,
                    FirmwareFlavor::LegacyVs2022,
                    "RELEASE-X64-VS2022-artifacts.tar.gz",
                    "firmware-RELEASE-X64-CLANGPDB-patina.tar.gz",
                ),
                (
                    CommonArch::Aarch64,
                    FirmwareFlavor::LegacyClangPdb,
                    "RELEASE-AARCH64-CLANGPDB-artifacts.tar.gz",
                    "firmware-RELEASE-AARCH64-CLANGPDB-patina.tar.gz",
                ),
            ] {
                assert_eq!(legacy_flavor.file_name(arch).unwrap(), legacy);
                assert_eq!(
                    FirmwareFlavor::PatinaClangPdb.file_name(arch).unwrap(),
                    patina
                );
            }
            assert_eq!(
                FirmwareFlavor::LegacyClangPdb
                    .file_name(CommonArch::X86_64)
                    .unwrap(),
                "firmware-RELEASE-X64-CLANGPDB.tar.gz"
            );
            assert!(
                FirmwareFlavor::LegacyVs2022
                    .file_name(CommonArch::Aarch64)
                    .is_err()
            );
        }

        #[test]
        fn requires_unique_patina_asset() {
            for arch in ["X64", "AARCH64"] {
                let file_name = format!("firmware-RELEASE-{arch}-CLANGPDB-patina.tar.gz");
                let asset = || Asset {
                    name: file_name.clone(),
                    browser_download_url: format!("https://example.com/{file_name}"),
                };
                let mut release = Release {
                    tag_name: "vtest".into(),
                    assets: vec![Asset {
                        name: "unrelated.tar.gz".into(),
                        browser_download_url: "https://example.com/unrelated.tar.gz".into(),
                    }],
                };
                assert!(
                    release
                        .unique_asset(&file_name)
                        .unwrap_err()
                        .to_string()
                        .contains("missing asset")
                );
                release.assets.push(asset());
                assert_eq!(
                    release
                        .unique_asset(&file_name)
                        .unwrap()
                        .browser_download_url,
                    asset().browser_download_url
                );
                release.assets.push(asset());
                assert!(
                    release
                        .unique_asset(&file_name)
                        .unwrap_err()
                        .to_string()
                        .contains("duplicate asset")
                );
            }
        }
    }
}
