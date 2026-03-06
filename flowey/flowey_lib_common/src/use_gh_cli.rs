// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Set up `gh` CLI for use with flowey.
//!
//! The executable this node returns will wrap the base `gh` cli executable with
//! some additional logic, notably, ensuring it is includes any necessary
//! authentication.

use flowey::node::prelude::*;
use std::io::Write;

#[derive(Serialize, Deserialize)]
pub enum GhCliAuth<C = VarNotClaimed> {
    /// Prompt user to log-in interactively.
    LocalOnlyInteractive,
    /// Set the value of the `GITHUB_TOKEN` environment variable to the
    /// specified runtime String when invoking the `gh` CLI.
    AuthToken(ReadVar<String, C>),
}

impl ClaimVar for GhCliAuth {
    type Claimed = GhCliAuth<VarClaimed>;

    fn claim(self, ctx: &mut StepCtx<'_>) -> Self::Claimed {
        match self {
            GhCliAuth::LocalOnlyInteractive => GhCliAuth::LocalOnlyInteractive,
            GhCliAuth::AuthToken(v) => GhCliAuth::AuthToken(v.claim(ctx)),
        }
    }
}

flowey_request! {
    pub enum Request {
        /// Specify what authentication to use
        WithAuth(GhCliAuth),
        /// Get a path to `gh` executable
        Get(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::download_gh_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut get_reqs = Vec::new();
        let mut with_auth_interactive = false;
        let mut with_auth_token = None;

        for req in requests {
            match req {
                Request::WithAuth(v) => match v {
                    GhCliAuth::LocalOnlyInteractive => with_auth_interactive = true,
                    GhCliAuth::AuthToken(v) => {
                        same_across_all_reqs_backing_var("WithAuth", &mut with_auth_token, v)?
                    }
                },
                Request::Get(v) => get_reqs.push(v),
            }
        }

        let get_reqs = get_reqs;
        let auth = match (with_auth_interactive, with_auth_token) {
            (true, None) => GhCliAuth::LocalOnlyInteractive,
            (false, Some(v)) => GhCliAuth::AuthToken(v),
            (true, Some(_)) => {
                anyhow::bail!("`WithAuth` must be consistent across requests")
            }
            (false, None) => anyhow::bail!("Missing essential request: WithAuth"),
        };

        // -- end of req processing -- //

        if get_reqs.is_empty() {
            if let GhCliAuth::AuthToken(tok) = auth {
                tok.claim_unused(ctx);
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
                    GhCliAuth::LocalOnlyInteractive => String::new(),
                    GhCliAuth::AuthToken(tok) => rt.read(tok),
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
