// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Update components in CCA emulation environment according to options specified.
use crate::_jobs::local_install_cca_emu::{build_cca_rootfs, build_plane0_linux};
use flowey::node::prelude::*;

#[derive(Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubCmds {
    pub rebuild_plane0_linux: bool,
    pub rebuild_rootfs: bool,
    pub tfa_rev: Option<String>,
    pub tfrmm_rev: Option<String>,
    pub plane0_linux_rev: Option<String>,
}

flowey_request! {
    pub struct Params {
        pub test_root: PathBuf,
        pub sub_cmds: SubCmds,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            test_root,
            sub_cmds,
            done,
        } = request;

        let SubCmds {
            rebuild_plane0_linux,
            rebuild_rootfs,
            tfa_rev: _,
            tfrmm_rev: _,
            plane0_linux_rev: _,
        } = sub_cmds;

        ctx.emit_rust_step("update cca emulation environment", |ctx| {
            done.claim(ctx);
            move |rt| {
                if rebuild_plane0_linux {
                    let plane0_linux = test_root.join("plane0-linux");
                    let plane0_image = plane0_linux
                        .join("arch")
                        .join("arm64")
                        .join("boot")
                        .join("Image");

                    anyhow::ensure!(
                        plane0_linux.exists(),
                        "plane0 Linux source tree is missing at {}, try --install-emu first",
                        plane0_linux.display()
                    );

                    build_plane0_linux(rt, &plane0_linux, &plane0_image)?;
                }

                if rebuild_rootfs {
                    let shrinkwrap_dir = test_root.join("shrinkwrap");
                    let venv_dir = shrinkwrap_dir.join("venv");
                    build_cca_rootfs(rt, &test_root, &shrinkwrap_dir, &venv_dir)?;
                }

                Ok(())
            }
        });

        Ok(())
    }
}
