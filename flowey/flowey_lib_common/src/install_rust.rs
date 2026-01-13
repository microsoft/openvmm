// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install a Rust toolchain, and ensure those tools are available on
//! the user's $PATH

use flowey::node::prelude::*;
use std::collections::BTreeSet;
use std::io::Write;

new_flow_node!(struct Node);

flowey_request! {
    pub enum Request {
        /// Automatically install all required Rust tools and components.
        ///
        /// If false - will check for pre-existing Rust installation, and fail
        /// if it doesn't meet the current job's requirements.
        AutoInstall(bool),

        /// Ignore the Version requirement, and build using whatever version of
        /// the Rust toolchain the user has installed locally.
        IgnoreVersion(bool),

        /// Install a specific Rust toolchain version.
        // FUTURE: support installing / using multiple versions of the Rust
        // toolchain at the same time, e.g: for stable and nightly, or to
        // support regression tests between a pinned rust version and current
        // stable.
        Version(String),

        /// Specify an additional target-triple to install the toolchain for.
        ///
        /// By default, only the native target will be installed.
        InstallTargetTriple(target_lexicon::Triple),

        /// If Rust was installed via Rustup, return the rustup toolchain that
        /// was installed (e.g: when specifting `+stable` or `+nightly` to
        /// commands)
        GetRustupToolchain(WriteVar<Option<String>>),

        /// Install the specified component.
        InstallComponent(String),

        /// Get the path to $CARGO_HOME
        GetCargoHome(WriteVar<PathBuf>),

        /// Ensure that Rust was installed and is available on the $PATH
        EnsureInstalled(WriteVar<SideEffect>),
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(dep: &mut ImportCtx<'_>) {
        dep.import::<crate::check_needs_relaunch::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut ensure_installed = Vec::new();
        let mut rust_toolchain = None;
        let mut auto_install = None;
        let mut ignore_version = None;
        let mut additional_target_triples = BTreeSet::new();
        let mut additional_components = BTreeSet::new();
        let mut get_rust_toolchain = Vec::new();
        let mut get_cargo_home = Vec::new();

        for req in requests {
            match req {
                Request::EnsureInstalled(v) => ensure_installed.push(v),
                Request::AutoInstall(v) => {
                    same_across_all_reqs("AutoInstall", &mut auto_install, v)?
                }
                Request::IgnoreVersion(v) => {
                    same_across_all_reqs("IgnoreVersion", &mut ignore_version, v)?
                }
                Request::Version(v) => same_across_all_reqs("Version", &mut rust_toolchain, v)?,
                Request::InstallTargetTriple(s) => {
                    additional_target_triples.insert(s.to_string());
                }
                Request::InstallComponent(v) => {
                    additional_components.insert(v);
                }
                Request::GetRustupToolchain(v) => get_rust_toolchain.push(v),
                Request::GetCargoHome(v) => get_cargo_home.push(v),
            }
        }

        let ensure_installed = ensure_installed;
        let auto_install =
            auto_install.ok_or(anyhow::anyhow!("Missing essential request: AutoInstall",))?;
        if !auto_install && matches!(ctx.backend(), FlowBackend::Github) {
            anyhow::bail!("`AutoInstall` must be true when using the Github backend");
        }
        let ignore_version =
            ignore_version.ok_or(anyhow::anyhow!("Missing essential request: IgnoreVersion",))?;
        if ignore_version && matches!(ctx.backend(), FlowBackend::Github) {
            anyhow::bail!("`IgnoreVersion` must be false when using the Github backend");
        }
        let rust_toolchain =
            rust_toolchain.ok_or(anyhow::anyhow!("Missing essential request: RustToolchain"))?;
        let additional_target_triples = additional_target_triples;
        let additional_components = additional_components;
        let get_rust_toolchain = get_rust_toolchain;
        let get_cargo_home = get_cargo_home;

        // -- end of req processing -- //

        let rust_toolchain = (!ignore_version).then_some(rust_toolchain);

        let check_rust_install = {
            let rust_toolchain = rust_toolchain.clone();
            let additional_target_triples = additional_target_triples.clone();
            let additional_components = additional_components.clone();

            move |_: &mut RustRuntimeServices<'_>| {
                if which::which("cargo").is_err() {
                    anyhow::bail!("did not find `cargo` on $PATH");
                }

                let rust_toolchain = rust_toolchain.map(|s| format!("+{s}"));
                let rust_toolchain = rust_toolchain.as_ref();

                // make sure the specific rust version was installed
                let sh = xshell::Shell::new()?;
                xshell::cmd!(sh, "rustc {rust_toolchain...} -vV").run()?;

                // make sure the additional target triples were installed
                if let Ok(rustup) = which::which("rustup") {
                    for (thing, expected_things) in [
                        ("target", &additional_target_triples),
                        ("component", &additional_components),
                    ] {
                        let output = xshell::cmd!(
                            sh,
                            "{rustup} {rust_toolchain...} {thing} list --installed"
                        )
                        .ignore_status()
                        .output()?;
                        let stderr = String::from_utf8(output.stderr)?;
                        let stdout = String::from_utf8(output.stdout)?;

                        // This error message may occur if the user has rustup
                        // installed, but is using a custom custom toolchain.
                        //
                        // NOTE: not thrilled that we are sniffing a magic string
                        // from stderr... but I'm also not sure if there's a better
                        // way to detect this...
                        if stderr.contains("does not support components") {
                            log::warn!("Detected a non-standard `rustup default` toolchain!");
                            log::warn!(
                                "Will not be able to double-check that all required target-triples and components are available."
                            );
                        } else {
                            let mut installed_things = BTreeSet::new();

                            for line in stdout.lines() {
                                let triple = line.trim();
                                installed_things.insert(triple);
                            }

                            for expected_thing in expected_things {
                                if !installed_things.contains(expected_thing.as_str()) {
                                    anyhow::bail!(
                                        "missing required {thing}: {expected_thing}; to install: `rustup {thing} add {expected_thing}`"
                                    )
                                }
                            }
                        }
                    }
                } else {
                    log::warn!("`rustup` was not found!");
                    log::warn!(
                        "Unable to double-check that all target-triples and components are available."
                    )
                }

                anyhow::Ok(())
            }
        };

        let check_is_installed = |write_cargo_bin: Option<
            WriteVar<Option<crate::check_needs_relaunch::BinOrEnv>>,
        >,
                                  ensure_installed: Vec<WriteVar<SideEffect>>,
                                  auto_install: bool,
                                  ctx: &mut NodeCtx<'_>| {
            if write_cargo_bin.is_some() || !ensure_installed.is_empty() {
                if auto_install || matches!(ctx.backend(), FlowBackend::Github) {
                    let added_to_path = if matches!(ctx.backend(), FlowBackend::Github) {
                        Some(ctx.emit_rust_step("add default cargo home to path", |_| {
                            |_| {
                                let default_cargo_home = home::home_dir()
                                    .context("Unable to get home dir")?
                                    .join(".cargo")
                                    .join("bin");
                                let github_path = std::env::var("GITHUB_PATH")?;
                                let mut github_path =
                                    fs_err::File::options().append(true).open(github_path)?;
                                github_path
                                    .write_all(default_cargo_home.as_os_str().as_encoded_bytes())?;
                                log::info!("Added {} to PATH", default_cargo_home.display());
                                Ok(())
                            }
                        }))
                    } else {
                        None
                    };

                    let rust_toolchain = rust_toolchain.clone();
                    ctx.emit_rust_step("install Rust", |ctx| {
                        let write_cargo_bin = if let Some(write_cargo_bin) = write_cargo_bin {
                            Some(write_cargo_bin.claim(ctx))
                        } else {
                            ensure_installed.claim(ctx);
                            None
                        };
                        added_to_path.claim(ctx);

                        move |rt: &mut RustRuntimeServices<'_>| {
                            if let Some(write_cargo_bin) = write_cargo_bin {
                                rt.write(write_cargo_bin, &Some(crate::check_needs_relaunch::BinOrEnv::Bin("cargo".to_string())));
                            }
                            let rust_toolchain = rust_toolchain.clone();
                            if check_rust_install.clone()(rt).is_ok() {
                                return Ok(());
                            }

                            let sh = xshell::Shell::new()?;
                            match rt.platform() {
                                FlowPlatform::Linux(_) => {
                                    let interactive_prompt = Some("-y");
                                    let mut default_toolchain = Vec::new();
                                    if let Some(ver) = rust_toolchain {
                                        default_toolchain.push("--default-toolchain".into());
                                        default_toolchain.push(ver)
                                    };

                                    xshell::cmd!(
                                        sh,
                                        "curl --fail --proto =https --tlsv1.2 -sSf https://sh.rustup.rs -o rustup-init.sh"
                                    )
                                    .run()?;
                                    xshell::cmd!(sh, "chmod +x ./rustup-init.sh").run()?;
                                    xshell::cmd!(
                                        sh,
                                        "./rustup-init.sh {interactive_prompt...} {default_toolchain...}"
                                    )
                                    .run()?;
                                }
                                FlowPlatform::Windows => {
                                    let interactive_prompt = Some("-y");
                                    let mut default_toolchain = Vec::new();
                                    if let Some(ver) = rust_toolchain {
                                        default_toolchain.push("--default-toolchain".into());
                                        default_toolchain.push(ver)
                                    };

                                    let arch = match rt.arch() {
                                        FlowArch::X86_64 => "x86_64",
                                        FlowArch::Aarch64 => "aarch64",
                                        arch => anyhow::bail!("unsupported arch {arch}"),
                                    };

                                    xshell::cmd!(
                                        sh,
                                        "curl --fail -sSfLo rustup-init.exe https://win.rustup.rs/{arch} --output rustup-init"
                                    ).run()?;
                                    xshell::cmd!(
                                        sh,
                                        "./rustup-init.exe {interactive_prompt...} {default_toolchain...}"
                                    )
                                    .run()?;
                                },
                                platform => anyhow::bail!("unsupported platform {platform}"),
                            }

                            if !additional_target_triples.is_empty() {
                                xshell::cmd!(sh, "rustup target add {additional_target_triples...}")
                                    .run()?;
                            }
                            if !additional_components.is_empty() {
                                xshell::cmd!(sh, "rustup component add {additional_components...}")
                                    .run()?;
                            }

                            Ok(())
                        }
                    })
                } else if let Some(write_cargo_bin) = write_cargo_bin {
                    ctx.emit_rust_step("ensure Rust is installed", |ctx| {
                        let write_cargo_bin = write_cargo_bin.claim(ctx);
                        move |rt| {
                            rt.write(
                                write_cargo_bin,
                                &Some(crate::check_needs_relaunch::BinOrEnv::Bin(
                                    "cargo".to_string(),
                                )),
                            );

                            check_rust_install(rt)?;
                            Ok(())
                        }
                    })
                } else {
                    ReadVar::from_static(()).into_side_effect()
                }
            } else {
                ReadVar::from_static(()).into_side_effect()
            }
        };

        // The reason we need to check for relaunch on Local but not GH Actions is that GH Actions
        // spawns a new shell for each step, so the new shell will have the new $PATH. On the local backend,
        // the same shell is reused and needs to be relaunched to pick up the new $PATH.
        let is_installed =
            if !ensure_installed.is_empty() && matches!(ctx.backend(), FlowBackend::Local) {
                let (read_bin, write_cargo_bin) = ctx.new_var();
                ctx.req(crate::check_needs_relaunch::Params {
                    check: read_bin,
                    done: ensure_installed,
                });
                check_is_installed(Some(write_cargo_bin), Vec::new(), auto_install, ctx)
            } else {
                check_is_installed(None, ensure_installed, auto_install, ctx)
            };

        if !get_rust_toolchain.is_empty() {
            ctx.emit_rust_step("detect active toolchain", |ctx| {
                is_installed.clone().claim(ctx);
                let get_rust_toolchain = get_rust_toolchain.claim(ctx);

                move |rt| {
                    let rust_toolchain = match rust_toolchain {
                        Some(toolchain) => Some(toolchain),
                        None => {
                            let sh = xshell::Shell::new()?;
                            if let Ok(rustup) = which::which("rustup") {
                                // Unfortunately, `rustup` still doesn't have any stable way to emit
                                // machine-readable output. See https://github.com/rust-lang/rustup/issues/450
                                //
                                // As a result, this logic is written to work with multiple rustup
                                // versions, both prior-to, and after 1.28.0.
                                //
                                // Prior to 1.28.0:
                                //   $ rustup show active-toolchain
                                //   stable-x86_64-unknown-linux-gnu (default)
                                //
                                // Starting from 1.28.0:
                                //   $ rustup show active-toolchain
                                //   stable-x86_64-unknown-linux-gnu
                                //   active because: it's the default toolchain
                                let output =
                                    xshell::cmd!(sh, "{rustup} show active-toolchain").output()?;
                                let stdout = String::from_utf8(output.stdout)?;
                                let line = stdout.lines().next().unwrap();
                                Some(line.split(' ').next().unwrap().into())
                            } else {
                                None
                            }
                        }
                    };

                    rt.write_all(get_rust_toolchain, &rust_toolchain);

                    Ok(())
                }
            });
        }

        if !get_cargo_home.is_empty() {
            ctx.emit_rust_step("report $CARGO_HOME", |ctx| {
                is_installed.claim(ctx);
                let get_cargo_home = get_cargo_home.claim(ctx);
                move |rt| {
                    let cargo_home = home::cargo_home()?;
                    rt.write_all(get_cargo_home, &cargo_home);

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
