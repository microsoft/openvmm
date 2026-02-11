// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generate a cargo-nextest run command.

use crate::run_cargo_build::CargoBuildProfile;
use crate::run_cargo_nextest_run::build_params;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::ffi::OsString;

flowey_request! {
    pub struct Request {
        /// What kind of test run this is (inline build vs. from nextest archive).
        pub run_kind_deps: RunKindDeps,
        /// Working directory the test archive was created from.
        pub working_dir: ReadVar<PathBuf>,
        /// Path to `.config/nextest.toml`
        pub config_file: ReadVar<PathBuf>,
        /// Path to any tool-specific config files
        pub tool_config_files: Vec<(String, ReadVar<PathBuf>)>,
        /// Nextest profile to use when running the source code (as defined in the
        /// `.config.nextest.toml`).
        pub nextest_profile: String,
        /// Nextest test filter expression
        pub nextest_filter_expr: Option<String>,
        /// Whether to run ignored tests
        pub run_ignored: bool,
        /// Override fail fast setting
        pub fail_fast: Option<bool>,
        /// Additional env vars set when executing the tests.
        pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
        /// Additional command to run before the tests
        pub extra_commands: Option<ReadVar<Vec<(OsString, Vec<OsString>)>>>,
        /// Generate a portable command with paths relative to `test_content_dir`
        pub portable: bool,
        /// Command for running the tests
        pub command: WriteVar<Script>,
    }
}

#[derive(Serialize, Deserialize)]
pub enum RunKindDeps<C = VarNotClaimed> {
    BuildAndRun {
        params: build_params::NextestBuildParams<C>,
        nextest_installed: ReadVar<SideEffect, C>,
        rust_toolchain: ReadVar<Option<String>, C>,
        cargo_flags: ReadVar<crate::cfg_cargo_common_flags::Flags, C>,
    },
    RunFromArchive {
        archive_file: ReadVar<PathBuf, C>,
        nextest_bin: ReadVar<PathBuf, C>,
        target: ReadVar<target_lexicon::Triple, C>,
    },
}

#[derive(Serialize, Deserialize)]
pub enum CommandShell {
    Powershell,
    Bash,
}

#[derive(Serialize, Deserialize)]
pub struct Script {
    pub env: BTreeMap<String, String>,
    pub commands: Vec<(OsString, Vec<OsString>)>,
    pub shell: CommandShell,
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_cargo_common_flags::Node>();
        ctx.import::<crate::download_cargo_nextest::Node>();
        ctx.import::<crate::install_cargo_nextest::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        for Request {
            run_kind_deps,
            working_dir,
            config_file,
            tool_config_files,
            nextest_profile,
            extra_env,
            extra_commands,
            nextest_filter_expr,
            run_ignored,
            fail_fast,
            portable,
            command,
        } in requests
        {
            ctx.emit_rust_step("generate nextest command", |ctx| {
                let run_kind_deps = run_kind_deps.claim(ctx);
                let working_dir = working_dir.claim(ctx);
                let config_file = config_file.claim(ctx);
                let tool_config_files = tool_config_files
                    .into_iter()
                    .map(|(a, b)| (a, b.claim(ctx)))
                    .collect::<Vec<_>>();
                let extra_env = extra_env.claim(ctx);
                let extra_commands = extra_commands.claim(ctx);
                let command = command.claim(ctx);

                move |rt| {
                    let working_dir = rt.read(working_dir);
                    let config_file = rt.read(config_file);
                    let mut with_env = rt.read(extra_env).unwrap_or_default();
                    let mut commands = rt.read(extra_commands).unwrap_or_default();

                    let target = match &run_kind_deps {
                        RunKindDeps::BuildAndRun {
                            params: build_params::NextestBuildParams { target, .. },
                            ..
                        } => target.clone(),
                        RunKindDeps::RunFromArchive { target, .. } => rt.read(target.clone()),
                    };

                    let windows_target = matches!(
                        target.operating_system,
                        target_lexicon::OperatingSystem::Windows
                    );
                    let windows_via_wsl2 = windows_target && crate::_util::running_in_wsl(rt);

                    let working_dir_ref = working_dir.as_path();
                    let working_dir_win = windows_via_wsl2.then(|| {
                        crate::_util::wslpath::linux_to_win(working_dir_ref)
                            .display()
                            .to_string()
                    });
                    let maybe_convert_path = |path: PathBuf| -> anyhow::Result<PathBuf> {
                        let path = if windows_via_wsl2 {
                            crate::_util::wslpath::linux_to_win(path)
                        } else {
                            path.absolute()
                                .with_context(|| format!("invalid path {}", path.display()))?
                        };
                        let path = if portable {
                            if windows_target {
                                let working_dir_trimmed =
                                    working_dir_win.as_ref().unwrap().trim_end_matches('\\');
                                let path_win = path.display().to_string();
                                let path_trimmed = path_win.trim_end_matches('\\');
                                PathBuf::from(format!(
                                    "$PSScriptRoot{}",
                                    path_trimmed
                                        .strip_prefix(working_dir_trimmed)
                                        .with_context(|| format!(
                                            "{} not in {}",
                                            path_win, working_dir_trimmed
                                        ),)?
                                ))
                            } else {
                                path.strip_prefix(working_dir_ref)
                                    .with_context(|| {
                                        format!(
                                            "{} not in {}",
                                            path.display(),
                                            working_dir_ref.display()
                                        )
                                    })?
                                    .to_path_buf()
                            }
                        } else {
                            path
                        };
                        Ok(path)
                    };

                    enum NextestInvocation {
                        // when tests are already built and provided via archive
                        Standalone { nextest_bin: PathBuf },
                        // when tests need to be compiled first
                        WithCargo { rust_toolchain: Option<String> },
                    }

                    // the invocation of `nextest run` is quite different
                    // depending on whether this is an archived run or not, as
                    // archives don't require passing build args (after all -
                    // those were passed when the archive was built), nor do
                    // they require having cargo installed.
                    let (nextest_invocation, build_args, build_env) = match run_kind_deps {
                        RunKindDeps::BuildAndRun {
                            params:
                                build_params::NextestBuildParams {
                                    packages,
                                    features,
                                    no_default_features,
                                    target,
                                    profile,
                                    extra_env,
                                },
                            nextest_installed: _, // side-effect
                            rust_toolchain,
                            cargo_flags,
                        } => {
                            let (mut build_args, build_env) = cargo_nextest_build_args_and_env(
                                rt.read(cargo_flags),
                                profile,
                                target,
                                rt.read(packages),
                                features,
                                no_default_features,
                                rt.read(extra_env),
                            );

                            let nextest_invocation = NextestInvocation::WithCargo {
                                rust_toolchain: rt.read(rust_toolchain),
                            };

                            // nextest also requires explicitly specifying the
                            // path to a cargo-metadata.json file when running
                            // using --workspace-remap (which do we below).
                            let cargo_metadata_path = std::env::current_dir()?
                                .absolute()?
                                .join("cargo_metadata.json");

                            rt.sh.change_dir(&working_dir);
                            let output =
                                flowey::shell_cmd!(rt, "cargo metadata --format-version 1")
                                    .output()?;
                            let cargo_metadata = String::from_utf8(output.stdout)?;
                            fs_err::write(&cargo_metadata_path, cargo_metadata)?;

                            build_args.push("--cargo-metadata".into());
                            build_args.push(cargo_metadata_path.display().to_string());

                            (nextest_invocation, build_args, build_env)
                        }
                        RunKindDeps::RunFromArchive {
                            archive_file,
                            nextest_bin,
                            target: _,
                        } => {
                            let build_args = vec![
                                "--archive-file".into(),
                                maybe_convert_path(rt.read(archive_file))?
                                    .display()
                                    .to_string(),
                            ];

                            let nextest_invocation = NextestInvocation::Standalone {
                                nextest_bin: rt.read(nextest_bin),
                            };

                            (nextest_invocation, build_args, BTreeMap::default())
                        }
                    };

                    let mut args: Vec<OsString> = Vec::new();

                    let argv0: OsString = match nextest_invocation {
                        NextestInvocation::Standalone { nextest_bin } => if portable {
                            maybe_convert_path(nextest_bin)?
                        } else {
                            nextest_bin
                        }
                        .into(),
                        NextestInvocation::WithCargo { rust_toolchain } => {
                            if let Some(rust_toolchain) = rust_toolchain {
                                args.extend(["run".into(), rust_toolchain.into(), "cargo".into()]);
                                "rustup".into()
                            } else {
                                "cargo".into()
                            }
                        }
                    };

                    args.extend([
                        "nextest".into(),
                        "run".into(),
                        "--profile".into(),
                        (&nextest_profile).into(),
                        "--config-file".into(),
                        maybe_convert_path(config_file)?.into(),
                        "--workspace-remap".into(),
                        maybe_convert_path(working_dir.clone())?.into(),
                    ]);

                    for (tool, config_file) in tool_config_files {
                        args.extend([
                            "--tool-config-file".into(),
                            format!(
                                "{}:{}",
                                tool,
                                maybe_convert_path(rt.read(config_file))?.display()
                            )
                            .into(),
                        ]);
                    }

                    args.extend(build_args.into_iter().map(Into::into));

                    if let Some(nextest_filter_expr) = nextest_filter_expr {
                        args.push("--filter-expr".into());
                        args.push(nextest_filter_expr.into());
                    }

                    if run_ignored {
                        args.push("--run-ignored".into());
                        args.push("all".into());
                    }

                    if let Some(fail_fast) = fail_fast {
                        if fail_fast {
                            args.push("--fail-fast".into());
                        } else {
                            args.push("--no-fail-fast".into());
                        }
                    }

                    // useful default to have
                    if !with_env.contains_key("RUST_BACKTRACE") {
                        with_env.insert("RUST_BACKTRACE".into(), "1".into());
                    }

                    // if running in CI, no need to waste time with incremental
                    // build artifacts
                    if !matches!(rt.backend(), FlowBackend::Local) {
                        with_env.insert("CARGO_INCREMENTAL".into(), "0".into());
                    }

                    // also update WSLENV in cases where we're running windows tests via WSL2
                    if !portable && crate::_util::running_in_wsl(rt) {
                        let old_wslenv = std::env::var("WSLENV");
                        let new_wslenv = with_env.keys().cloned().collect::<Vec<_>>().join(":");
                        with_env.insert(
                            "WSLENV".into(),
                            format!(
                                "{}{}",
                                old_wslenv.map(|s| s + ":").unwrap_or_default(),
                                new_wslenv
                            ),
                        );
                    }

                    // the build_env vars don't need to be mirrored to WSLENV,
                    // and so they are only injected after the WSLENV code has
                    // run.
                    with_env.extend(build_env);

                    commands.push((argv0, args));

                    rt.write(
                        command,
                        &Script {
                            env: with_env,
                            commands,
                            shell: if (portable || !windows_via_wsl2)
                                && matches!(
                                    target.operating_system,
                                    target_lexicon::OperatingSystem::Windows
                                ) {
                                CommandShell::Powershell
                            } else {
                                CommandShell::Bash
                            },
                        },
                    );

                    Ok(())
                }
            });
        }

        Ok(())
    }
}

// shared with `cargo_nextest_archive`
pub(crate) fn cargo_nextest_build_args_and_env(
    cargo_flags: crate::cfg_cargo_common_flags::Flags,
    cargo_profile: CargoBuildProfile,
    target: target_lexicon::Triple,
    packages: build_params::TestPackages,
    features: crate::run_cargo_build::CargoFeatureSet,
    no_default_features: bool,
    mut extra_env: BTreeMap<String, String>,
) -> (Vec<String>, BTreeMap<String, String>) {
    let locked = cargo_flags.locked.then_some("--locked");
    let verbose = cargo_flags.verbose.then_some("--verbose");
    let cargo_profile = match &cargo_profile {
        CargoBuildProfile::Debug => "dev",
        CargoBuildProfile::Release => "release",
        CargoBuildProfile::Custom(s) => s,
    };
    let target = target.to_string();

    let packages: Vec<String> = {
        // exclude benches
        let mut v = vec!["--tests".into(), "--bins".into()];

        match packages {
            build_params::TestPackages::Workspace { exclude } => {
                v.push("--workspace".into());
                for crate_name in exclude {
                    v.push("--exclude".into());
                    v.push(crate_name);
                }
            }
            build_params::TestPackages::Crates { crates } => {
                for crate_name in crates {
                    v.push("-p".into());
                    v.push(crate_name);
                }
            }
        }

        v
    };

    let mut args = Vec::new();
    args.extend(locked.map(Into::into));
    args.extend(verbose.map(Into::into));
    args.push("--cargo-profile".into());
    args.push(cargo_profile.into());
    args.push("--target".into());
    args.push(target);
    args.extend(packages);
    if no_default_features {
        args.push("--no-default-features".into())
    }
    args.extend(features.to_cargo_arg_strings());

    let mut env = BTreeMap::new();

    env.append(&mut extra_env);

    (args, env)
}

// FUTURE: this seems like something a proc-macro can help with...
impl RunKindDeps {
    pub fn claim(self, ctx: &mut StepCtx<'_>) -> RunKindDeps<VarClaimed> {
        match self {
            RunKindDeps::BuildAndRun {
                params,
                nextest_installed,
                rust_toolchain,
                cargo_flags,
            } => RunKindDeps::BuildAndRun {
                params: params.claim(ctx),
                nextest_installed: nextest_installed.claim(ctx),
                rust_toolchain: rust_toolchain.claim(ctx),
                cargo_flags: cargo_flags.claim(ctx),
            },
            RunKindDeps::RunFromArchive {
                archive_file,
                nextest_bin,
                target,
            } => RunKindDeps::RunFromArchive {
                archive_file: archive_file.claim(ctx),
                nextest_bin: nextest_bin.claim(ctx),
                target: target.claim(ctx),
            },
        }
    }
}

impl std::fmt::Display for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let quote_char = match self.shell {
            CommandShell::Powershell => "\"",
            CommandShell::Bash => "'",
        };

        let env_string = match self.shell {
            CommandShell::Powershell => self
                .env
                .iter()
                .map(|(k, v)| format!("$env:{k}=\"{v}\""))
                .collect::<Vec<_>>()
                .join("\n"),
            CommandShell::Bash => self
                .env
                .iter()
                .map(|(k, v)| format!("export {k}=\"{v}\""))
                .collect::<Vec<_>>()
                .join("\n"),
        };
        writeln!(f, "{env_string}")?;

        for cmd in &self.commands {
            let argv0_string = cmd.0.to_string_lossy();
            let argv0_string = match self.shell {
                CommandShell::Powershell => format!("&\"{argv0_string}\""),
                CommandShell::Bash => format!("\"{argv0_string}\""),
            };

            let arg_string = {
                cmd.1
                    .iter()
                    .map(|v| format!("{quote_char}{}{quote_char}", v.to_string_lossy()))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            writeln!(f, "{argv0_string} {arg_string}")?;
        }

        Ok(())
    }
}
