// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Set up `gh` CLI for use with flowey.
//!
//! The executable this node returns will wrap the base `gh` cli executable with
//! some additional logic, notably, ensuring it is includes any necessary
//! authentication.

use flowey::node::prelude::*;
use std::io::Write;

/// Auth config for the gh CLI node. Uses [`ConfigVar`] so that
/// `PartialEq`-based config merging works for the `ReadVar` variant.
#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
pub enum GhCliAuth {
    /// Prompt user to log-in interactively.
    #[default]
    LocalOnlyInteractive,
    /// Set the value of the `GITHUB_TOKEN` environment variable.
    AuthToken(ConfigVar<String>),
}

#[derive(Serialize, Deserialize)]
#[doc(hidden)]
pub enum ClaimedGhCliAuth {
    LocalOnlyInteractive,
    AuthToken(ClaimedReadVar<String>),
}

impl ClaimVar for GhCliAuth {
    type Claimed = ClaimedGhCliAuth;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        match self {
            GhCliAuth::LocalOnlyInteractive => ClaimedGhCliAuth::LocalOnlyInteractive,
            GhCliAuth::AuthToken(v) => ClaimedGhCliAuth::AuthToken(v.claim(ctx)),
        }
    }
}

flowey_config! {
    /// Config for the use_gh_cli node.
    pub struct Config {
        /// Specify what authentication to use
        pub auth: Option<GhCliAuth>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get a path to `gh` executable
        Get(WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_gh_cli::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut get_reqs = Vec::new();

        for req in requests {
            match req {
                Request::Get(v) => get_reqs.push(v),
            }
        }

        let auth = config.auth.ok_or(anyhow::anyhow!("missing config: auth"))?;
        let get_reqs = get_reqs;

        // -- end of req processing -- //

        if get_reqs.is_empty() {
            if let GhCliAuth::AuthToken(tok) = auth {
                tok.0.claim_unused(ctx);
            }
            return Ok(());
        }

        if !matches!(ctx.backend(), FlowBackend::Local) {
            if matches!(auth, GhCliAuth::LocalOnlyInteractive) {
                anyhow::bail!("cannot use interactive auth on a non-local backend")
            }
        }

        let gh_bin_path = ctx.reqv(crate::download_gh_cli::Request::Get);

        ctx.emit_rust_step("setup gh cli", |ctx| {
            let auth = auth.claim(ctx);
            let get_reqs = get_reqs.claim(ctx);
            let gh_bin_path = gh_bin_path.claim(ctx);
            |rt| {
                let gh_bin_path = rt.read(gh_bin_path).display().to_string();
                let gh_token = match auth {
                    ClaimedGhCliAuth::LocalOnlyInteractive => String::new(),
                    ClaimedGhCliAuth::AuthToken(tok) => rt.read(tok),
                };
                // only set GITHUB_TOKEN if there is a value to set it to, otherwise
                // let the user's environment take precedence over authenticating interactively
                let gh_token = if !gh_token.is_empty() {
                    match rt.platform().kind() {
                        FlowPlatformKind::Windows => format!(r#"SET "GITHUB_TOKEN={gh_token}""#),
                        FlowPlatformKind::Unix => format!(r#"GITHUB_TOKEN="{gh_token}""#),
                    }
                } else {
                    String::new()
                };

                let shim_txt = match rt.platform().kind() {
                    FlowPlatformKind::Windows => WINDOWS_SHIM_BAT.trim(),
                    FlowPlatformKind::Unix => UNIX_SHIM_SH.trim(),
                }
                .replace("{GITHUB_TOKEN}", &gh_token)
                .replace("{GH_BIN_PATH}", &gh_bin_path);

                let script_name = match rt.platform().kind() {
                    FlowPlatformKind::Windows => "shim.bat",
                    FlowPlatformKind::Unix => "shim.sh",
                };
                let path = {
                    let dst = std::env::current_dir()?.join(script_name);
                    let mut options = fs_err::OpenOptions::new();
                    #[cfg(unix)]
                    fs_err::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o777); // executable
                    let mut file = options.create_new(true).write(true).open(&dst)?;
                    file.write_all(shim_txt.as_bytes())?;
                    dst.absolute()?
                };
                if !flowey::shell_cmd!(rt, "{path} auth status")
                    .ignore_status()
                    .output()?
                    .status
                    .success()
                {
                    if matches!(rt.backend(), FlowBackend::Local) {
                        flowey::shell_cmd!(rt, "{path} auth login").run()?;
                    } else {
                        anyhow::bail!("unable to authenticate with github - is GhCliAuth valid?")
                    }
                };

                for var in get_reqs {
                    rt.write(var, &path);
                }

                Ok(())
            }
        });

        Ok(())
    }
}

const UNIX_SHIM_SH: &str = r#"
#!/bin/sh
{GITHUB_TOKEN} exec {GH_BIN_PATH} "$@"
"#;

const WINDOWS_SHIM_BAT: &str = r#"
@ECHO OFF
{GITHUB_TOKEN}
{GH_BIN_PATH} %*
"#;
