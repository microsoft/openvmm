// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download the mutable VMM.Perf runtime package.

use crate::common::CommonArch;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub enum Request {
        Get {
            arch: CommonArch,
            runtime_archive: WriteVar<PathBuf>,
        }
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut requests_by_arch = BTreeMap::<_, Vec<_>>::new();
        for Request::Get {
            arch,
            runtime_archive,
        } in requests
        {
            requests_by_arch
                .entry(arch)
                .or_default()
                .push(runtime_archive);
        }

        if requests_by_arch.is_empty() {
            return Ok(());
        }

        let azcopy = ctx.reqv(flowey_lib_common::download_azcopy::Request::GetAzCopy);
        let persistent_dir = ctx.persistent_dir();

        for (arch, outputs) in requests_by_arch {
            let filename = match arch {
                CommonArch::X86_64 => "vmm-perf-linux-x64.tar.gz",
                CommonArch::Aarch64 => "vmm-perf-linux-arm64.tar.gz",
            };
            let url = format!(
                "https://vmmperfartifactpublic.blob.core.windows.net/perfpackage/stable/{filename}"
            );

            ctx.emit_rust_step(
                format!(
                    "download VMM.Perf runtime ({})",
                    match arch {
                        CommonArch::X86_64 => "x64",
                        CommonArch::Aarch64 => "arm64",
                    }
                ),
                |ctx| {
                    let azcopy = azcopy.clone().claim(ctx);
                    let persistent_dir = persistent_dir.clone().claim(ctx);
                    let outputs = outputs.claim(ctx);
                    move |rt| {
                        let cache_dir = if let Some(dir) = persistent_dir {
                            rt.read(dir).join("vmm-perf")
                        } else {
                            rt.sh.current_dir().join("vmm-perf")
                        };
                        fs_err::create_dir_all(&cache_dir)?;
                        let archive = cache_dir.join(filename);
                        let azcopy = rt.read(azcopy);

                        flowey::shell_cmd!(
                            rt,
                            "{azcopy} copy
                                {url}
                                {archive}
                                --overwrite ifSourceNewer
                                --skip-version-check"
                        )
                        .run()?;

                        for output in outputs {
                            rt.write(output, &archive.absolute()?);
                        }
                        Ok(())
                    }
                },
            );
        }

        Ok(())
    }
}
