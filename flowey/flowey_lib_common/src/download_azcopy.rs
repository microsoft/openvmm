// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download a copy of `azcopy`

use crate::cache::CacheHit;
use crate::cache::CacheResult;
use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Get a path to `azcopy`
        GetAzCopy(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_dist_pkg::Node>();
        ctx.import::<crate::cache::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut get_azcopy = Vec::new();

        for req in requests {
            match req {
                Request::GetAzCopy(v) => get_azcopy.push(v),
            }
        }

        let get_azcopy = get_azcopy;

        // -- end of req processing -- //

        if get_azcopy.is_empty() {
            return Ok(());
        }

        let azcopy_bin = ctx.platform().binary("azcopy");

        let cache_dir = ctx.emit_rust_stepv("create azcopy cache dir", |_| {
            |_| Ok(std::env::current_dir()?.absolute()?)
        });

        let cache_key = ReadVar::from_static(format!("azcopy"));
        let hitvar = ctx.reqv(|hitvar| crate::cache::Request {
            label: "azcopy".into(),
            dir: cache_dir.clone(),
            key: cache_key,
            restore_keys: None,
            hitvar: CacheResult::HitVar(hitvar),
        });

        // in case we need to unzip the thing we downloaded
        let platform = ctx.platform();
        let bsdtar_installed = ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
            package_names: match platform {
                FlowPlatform::Linux(linux_distribution) => match linux_distribution {
                    FlowPlatformLinuxDistro::Fedora => vec!["bsdtar".into()],
                    FlowPlatformLinuxDistro::Ubuntu => vec!["libarchive-tools".into()],
                    FlowPlatformLinuxDistro::Unknown => vec![],
                },
                _ => {
                    vec![]
                }
            },
            done: v,
        });

        ctx.emit_rust_step("installing azcopy", |ctx| {
            bsdtar_installed.claim(ctx);
            let cache_dir = cache_dir.claim(ctx);
            let hitvar = hitvar.claim(ctx);
            let get_azcopy = get_azcopy.claim(ctx);
            move |rt| {
                let cache_dir = rt.read(cache_dir);

                let cached = if matches!(rt.read(hitvar), CacheHit::Hit) {
                    let cached_bin = cache_dir.join(&azcopy_bin);
                    assert!(cached_bin.exists());
                    Some(cached_bin)
                } else {
                    None
                };


                let path_to_azcopy = if let Some(cached) = cached {
                    cached
                } else {
                    let sh = xshell::Shell::new()?;
                    match rt.platform().kind() {
                        FlowPlatformKind::Windows => {
                            xshell::cmd!(sh, "curl --fail -L https://aka.ms/downloadazcopy-v10-windows -o azcopy.zip").run()?;

                            let bsdtar = crate::_util::bsdtar_name(rt);
                            xshell::cmd!(sh, "{bsdtar} -xf azcopy.zip --strip-components=1").run()?;
                        }
                        FlowPlatformKind::Unix => {
                            xshell::cmd!(sh, "curl --fail -L https://aka.ms/downloadazcopy-v10-linux -o azcopy.tar.gz").run()?;
                            xshell::cmd!(sh, "tar -xf azcopy.tar.gz --strip-components=1").run()?;
                        }
                    };

                    // move the unzipped bin into the cache dir
                    let final_bin = cache_dir.join(&azcopy_bin);
                    fs_err::rename(&azcopy_bin, &final_bin)?;

                    final_bin.absolute()?
                };

                for var in get_azcopy {
                    rt.write(var, &path_to_azcopy)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
