// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Public ARM64 MSHV support packages, separate from user-supplied runtime files.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

/// Public support package from openvmm-deps.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MshvSupport {
    Sbsa,
    Hvloader,
}

impl MshvSupport {
    fn package(self) -> &'static str {
        match self {
            Self::Sbsa => "sbsa-mshv",
            Self::Hvloader => "hvloader",
        }
    }

    fn files(self) -> &'static [&'static str] {
        match self {
            Self::Sbsa => &["SBSA_FLASH0.fd", "SBSA_FLASH1.fd", "manifest.txt"],
            Self::Hvloader => &[
                "HvLoader.efi",
                "LICENSE.HvLoader",
                "LICENSE.edk2",
                "manifest.txt",
            ],
        }
    }
}

flowey_config! {
    pub struct Config {
        /// Published openvmm-deps release containing both packages.
        pub version: Option<String>,
        /// Local extracted package overrides.
        pub local_paths: BTreeMap<MshvSupport, ConfigVar<PathBuf>>,
    }
}

flowey_request! {
    pub struct Request {
        pub package: MshvSupport,
        pub directory: WriteVar<PathBuf>,
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_gh_release::Node>();
    }

    fn emit(config: Config, requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut outputs: BTreeMap<MshvSupport, Vec<WriteVar<PathBuf>>> = BTreeMap::new();
        for Request { package, directory } in requests {
            outputs.entry(package).or_default().push(directory);
        }
        if outputs.is_empty() {
            return Ok(());
        }
        let persistent_dir = ctx.persistent_dir();
        for (package, outputs) in outputs {
            let (source, version) = if let Some(ConfigVar(path)) = config.local_paths.get(&package)
            {
                (path.clone(), None)
            } else {
                let version = config.version.clone().with_context(|| {
                    format!(
                        "no release configured for ARM64 {} support",
                        package.package()
                    )
                })?;
                let source = ctx.reqv(|path| flowey_lib_common::download_gh_release::Request {
                    repo_owner: crate::common::OPENVMM_GITHUB_OWNER.into(),
                    repo_name: crate::common::OPENVMM_DEPS_GITHUB_REPO.into(),
                    needs_auth: false,
                    tag: version.clone(),
                    file_name: format!(
                        "openvmm-test-{}.aarch64.{version}.tar.gz",
                        package.package()
                    ),
                    path,
                });
                (source, Some(version))
            };
            ctx.emit_rust_step("resolve public ARM64 MSHV support", |ctx| {
                let source = source.claim(ctx);
                let persistent_dir = persistent_dir.clone().claim(ctx);
                let outputs = outputs.claim(ctx);
                move |rt| {
                    let source = rt.read(source);
                    let directory = if let Some(version) = version {
                        let persistent_dir = persistent_dir.map(|dir| rt.read(dir));
                        flowey_lib_common::_util::extract::extract_tar_gz_if_new(
                            rt,
                            persistent_dir.as_deref(),
                            &source,
                            &version,
                        )?
                    } else {
                        source
                    };
                    for file in package.files() {
                        anyhow::ensure!(
                            directory.join(file).is_file(),
                            "missing public MSHV support file {}",
                            directory.join(file).display()
                        );
                    }
                    rt.write_all(outputs, &directory);
                    Ok(())
                }
            });
        }
        Ok(())
    }
}
