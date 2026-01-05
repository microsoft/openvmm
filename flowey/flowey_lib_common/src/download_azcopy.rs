// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `azcopy`

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Version of `azcopy` to install (e.g: "v10.31.0")
        Version(String),
        /// Get a path to `azcopy`
        GetAzCopy(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::download_gh_release::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut version = None;
        let mut get_azcopy = Vec::new();

        for req in requests {
            match req {
                Request::Version(v) => same_across_all_reqs("Version", &mut version, v)?,
                Request::GetAzCopy(v) => get_azcopy.push(v),
            }
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: Version"))?;
        let get_azcopy = get_azcopy;

        // -- end of req processing -- //

        if get_azcopy.is_empty() {
            return Ok(());
        }

        let azcopy_bin = ctx.platform().binary("azcopy");

        // in case we need to unzip the thing we downloaded
        let platform = ctx.platform();
        let bsdtar_installed = ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
            package_names: match platform {
                FlowPlatform::Linux(linux_distribution) => match linux_distribution {
                    FlowPlatformLinuxDistro::Fedora => vec!["bsdtar".into()],
                    FlowPlatformLinuxDistro::Ubuntu => vec!["libarchive-tools".into()],
                    FlowPlatformLinuxDistro::Arch => vec!["libarchive".into()],
                    FlowPlatformLinuxDistro::Unknown => vec![],
                },
                _ => {
                    vec![]
                }
            },
            done: v,
        });

        // Extract version number without 'v' prefix for file name (e.g., "v10.31.0" -> "10.31.0")
        let version_number = version.strip_prefix('v').unwrap_or(&version);

        // Determine file name at emit time based on platform/arch
        let (file_name, is_tar) = {
            let arch = match ctx.arch() {
                FlowArch::X86_64 => "amd64",
                FlowArch::Aarch64 => "arm64",
                _ => unreachable!("unsupported arch"),
            };
            match ctx.platform() {
                FlowPlatform::Windows => {
                    (format!("azcopy_windows_{arch}_{version_number}.zip"), false)
                }
                FlowPlatform::Linux(_) => {
                    (format!("azcopy_linux_{arch}_{version_number}.tar.gz"), true)
                }
                FlowPlatform::MacOs => {
                    (format!("azcopy_darwin_{arch}_{version_number}.zip"), false)
                }
                _ => unreachable!("unsupported platform"),
            }
        };

        let azcopy_archive = ctx.reqv(|v| crate::download_gh_release::Request {
            repo_owner: "Azure".to_string(),
            repo_name: "azure-storage-azcopy".to_string(),
            needs_auth: false,
            tag: version.clone(),
            file_name,
            path: v,
        });

        ctx.emit_rust_step("extract azcopy from archive", |ctx| {
            bsdtar_installed.claim(ctx);
            let get_azcopy = get_azcopy.claim(ctx);
            let azcopy_archive = azcopy_archive.claim(ctx);
            let azcopy_bin = azcopy_bin.clone();
            move |rt| {
                let azcopy_archive = rt.read(azcopy_archive);

                let sh = xshell::Shell::new()?;
                sh.change_dir(azcopy_archive.parent().unwrap());

                if is_tar {
                    xshell::cmd!(sh, "tar -xf {azcopy_archive} --strip-components=1").run()?;
                } else {
                    let bsdtar = crate::_util::bsdtar_name(rt);
                    xshell::cmd!(sh, "{bsdtar} -xf {azcopy_archive} --strip-components=1").run()?;
                }

                let path_to_azcopy = azcopy_archive
                    .parent()
                    .unwrap()
                    .join(&azcopy_bin)
                    .absolute()?;

                for var in get_azcopy {
                    rt.write(var, &path_to_azcopy)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
