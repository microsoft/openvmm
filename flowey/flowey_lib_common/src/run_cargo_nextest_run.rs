// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest tests.

use crate::run_cargo_build::CargoBuildProfile;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::ffi::OsString;

#[derive(Serialize, Deserialize)]
pub struct TestResults {
    pub all_tests_passed: bool,
    /// Path to JUnit XML output (if enabled by the nextest profile)
    pub junit_xml: Option<PathBuf>,
}

/// Parameters related to building nextest tests
pub mod build_params {
    use crate::run_cargo_build::CargoBuildProfile;
    use flowey::node::prelude::*;
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub enum PanicAbortTests {
        /// Assume the current rust toolchain is nightly
        // FUTURE: current flowey infrastructure doesn't actually have a path for
        // multi-toolchain drifting
        UsingNightly,
        /// Build with `RUSTC_BOOTSTRAP=1` set
        UsingRustcBootstrap,
    }

    #[derive(Serialize, Deserialize)]
    pub enum FeatureSet {
        All,
        Specific(Vec<String>),
    }

    /// Types of things that can be documented
    #[derive(Serialize, Deserialize)]
    pub enum TestPackages {
        /// Document an entire workspace workspace (with exclusions)
        Workspace {
            /// Exclude certain crates
            exclude: Vec<String>,
        },
        /// Document a specific set of crates.
        Crates {
            /// Crates to document
            crates: Vec<String>,
        },
    }

    #[derive(Serialize, Deserialize)]
    pub struct NextestBuildParams<C = VarNotClaimed> {
        /// Packages to test for
        pub packages: ReadVar<TestPackages, C>,
        /// Cargo features to enable when building
        pub features: FeatureSet,
        /// Whether to disable default features
        pub no_default_features: bool,
        /// Whether to build tests with unstable `-Zpanic-abort-tests` flag
        pub unstable_panic_abort_tests: Option<PanicAbortTests>,
        /// Build unit tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build unit tests with the specified cargo profile
        pub profile: CargoBuildProfile,
        /// Additional env vars set when building the tests
        pub extra_env: ReadVar<BTreeMap<String, String>, C>,
    }
}

/// Nextest run mode to use
#[derive(Serialize, Deserialize)]
pub enum NextestRunKind {
    /// Build and run tests in a single step.
    BuildAndRun(build_params::NextestBuildParams),
    /// Run tests from pre-built nextest archive file.
    RunFromArchive(ReadVar<PathBuf>),
}

#[derive(Serialize, Deserialize)]
pub struct Run {
    /// Friendly name for this test group that will be displayed in logs.
    pub friendly_name: String,
    /// What kind of test run this is (inline build vs. from nextest archive).
    pub run_kind: NextestRunKind,
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
    /// Set rlimits to allow unlimited sized coredump file (if supported)
    pub with_rlimit_unlimited_core_size: bool,
    /// Additional env vars set when executing the tests.
    pub extra_env: Option<ReadVar<BTreeMap<String, String>>>,
    /// Wait for specified side-effects to resolve before building / running any
    /// tests. (e.g: to allow for some ambient packages / dependencies to
    /// get installed).
    pub pre_run_deps: Vec<ReadVar<SideEffect>>,
    /// Results of running the tests
    pub results: WriteVar<TestResults>,
}

flowey_request! {
    pub enum Request {
        /// Set the default nextest fast fail behavior. Defaults to not
        /// fast-failing when a single test fails.
        DefaultNextestFailFast(bool),
        /// Set the default behavior when a test failure is encountered.
        /// Defaults to not terminating the job when a single test fails.
        DefaultTerminateJobOnFail(bool),
        Run(Run),
    }
}

enum RunKindDeps<C = VarNotClaimed> {
    BuildAndRun {
        params: build_params::NextestBuildParams<C>,
        nextest_installed: ReadVar<SideEffect, C>,
        rust_toolchain: ReadVar<Option<String>, C>,
        cargo_flags: ReadVar<crate::cfg_cargo_common_flags::Flags, C>,
    },
    RunFromArchive {
        archive_file: ReadVar<PathBuf, C>,
        nextest_bin: ReadVar<PathBuf, C>,
    },
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_cargo_common_flags::Node>();
        ctx.import::<crate::download_cargo_nextest::Node>();
        ctx.import::<crate::install_rust::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut run = Vec::new();
        let mut fail_fast = None;
        let mut terminate_job_on_fail = None;

        for req in requests {
            match req {
                Request::DefaultNextestFailFast(v) => {
                    same_across_all_reqs("OverrideFailFast", &mut fail_fast, v)?
                }
                Request::DefaultTerminateJobOnFail(v) => {
                    same_across_all_reqs("TerminateJobOnFail", &mut terminate_job_on_fail, v)?
                }
                Request::Run(v) => run.push(v),
            }
        }

        let terminate_job_on_fail = terminate_job_on_fail.unwrap_or(false);

        for Run {
            friendly_name,
            run_kind,
            working_dir,
            config_file,
            tool_config_files,
            nextest_profile,
            extra_env,
            with_rlimit_unlimited_core_size,
            nextest_filter_expr,
            run_ignored,
            pre_run_deps,
            results,
        } in run
        {
            let run_kind_deps = match run_kind {
                NextestRunKind::BuildAndRun(params) => {
                    let cargo_flags = ctx.reqv(crate::cfg_cargo_common_flags::Request::GetFlags);

                    let nextest_installed =
                        ctx.reqv(crate::download_cargo_nextest::Request::InstallWithCargo);

                    let rust_toolchain = ctx.reqv(crate::install_rust::Request::GetRustupToolchain);

                    ctx.req(crate::install_rust::Request::InstallTargetTriple(
                        params.target.clone(),
                    ));

                    RunKindDeps::BuildAndRun {
                        params,
                        nextest_installed,
                        rust_toolchain,
                        cargo_flags,
                    }
                }
                NextestRunKind::RunFromArchive(archive_file) => {
                    let nextest_bin =
                        ctx.reqv(crate::download_cargo_nextest::Request::InstallStandalone);

                    RunKindDeps::RunFromArchive {
                        archive_file,
                        nextest_bin,
                    }
                }
            };

            let (all_tests_passed_read, all_tests_passed_write) = ctx.new_var();
            let (junit_xml_read, junit_xml_write) = ctx.new_var();

            ctx.emit_rust_step(format!("run '{friendly_name}' nextest tests"), |ctx| {
                pre_run_deps.claim(ctx);

                let run_kind_deps = run_kind_deps.claim(ctx);
                let working_dir = working_dir.claim(ctx);
                let config_file = config_file.claim(ctx);
                let tool_config_files = tool_config_files
                    .into_iter()
                    .map(|(a, b)| (a, b.claim(ctx)))
                    .collect::<Vec<_>>();
                let extra_env = extra_env.claim(ctx);
                let all_tests_passed_var = all_tests_passed_write.claim(ctx);
                let junit_xml_write = junit_xml_write.claim(ctx);
                move |rt| {
                    let working_dir = rt.read(working_dir);
                    let config_file = rt.read(config_file);
                    let mut with_env = extra_env.map(|x| rt.read(x)).unwrap_or_default();

                    // first things first - determine if junit is supported by
                    // the profile, and if so, where the output if going to be.
                    let junit_path = {
                        let nextest_toml = fs_err::read_to_string(&config_file)?
                            .parse::<toml_edit::DocumentMut>()
                            .context("failed to parse nextest.toml")?;

                        let path = Some(&nextest_toml)
                            .and_then(|i| i.get("profile"))
                            .and_then(|i| i.get(&nextest_profile))
                            .and_then(|i| i.get("junit"))
                            .and_then(|i| i.get("path"));

                        if let Some(path) = path {
                            let path: PathBuf =
                                path.as_str().context("malformed nextest.toml")?.into();
                            Some(path)
                        } else {
                            None
                        }
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
                                    unstable_panic_abort_tests,
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
                                unstable_panic_abort_tests,
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

                            let sh = xshell::Shell::new()?;
                            sh.change_dir(&working_dir);
                            let output =
                                xshell::cmd!(sh, "cargo metadata --format-version 1").output()?;
                            let cargo_metadata = String::from_utf8(output.stdout)?;
                            fs_err::write(&cargo_metadata_path, cargo_metadata)?;

                            build_args.push("--cargo-metadata".into());
                            build_args.push(cargo_metadata_path.display().to_string());

                            (nextest_invocation, build_args, build_env)
                        }
                        RunKindDeps::RunFromArchive {
                            archive_file,
                            nextest_bin,
                        } => {
                            let build_args = vec![
                                "--archive-file".into(),
                                rt.read(archive_file).display().to_string(),
                            ];

                            let nextest_invocation = NextestInvocation::Standalone {
                                nextest_bin: rt.read(nextest_bin),
                            };

                            (nextest_invocation, build_args, BTreeMap::default())
                        }
                    };

                    let mut args: Vec<OsString> = Vec::new();

                    let argv0: OsString = match nextest_invocation {
                        NextestInvocation::Standalone { nextest_bin } => nextest_bin.into(),
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
                        config_file.into(),
                        "--workspace-remap".into(),
                        (&working_dir).into(),
                    ]);

                    for (tool, config_file) in tool_config_files {
                        args.extend([
                            "--tool-config-file".into(),
                            format!("{}:{}", tool, rt.read(config_file).display()).into(),
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
                    if crate::_util::running_in_wsl(rt) {
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

                    // allow unlimited coredump sizes
                    //
                    // FUTURE: would be cool if `flowey` had the ability to pass
                    // around "callbacks" as part of a, which would subsume the
                    // need to support things like `with_env` and
                    // `with_rlimit_unlimited_core_size`.
                    //
                    // This _should_ be doable using the same sort of mechanism
                    // that regular flowey Rust-based steps get registered +
                    // invoked. i.e: the serializable "callback" object is just
                    // a unique identifier for a set of
                    // (NodeHandle,callback_idx,requests), which flowey can use
                    // to "play-through" the specified node in order to get the
                    // caller a handle to a concrete `Box<dyn Fn...>`.
                    //
                    // I suspect there'll need to be some `Any` involved to get
                    // things to line up... but honestly, this seems doable?
                    // Will need to find time to experiment with this...
                    #[cfg(unix)]
                    let old_core_rlimits = if with_rlimit_unlimited_core_size
                        && matches!(rt.platform(), FlowPlatform::Linux(_))
                    {
                        let limits = rlimit::getrlimit(rlimit::Resource::CORE)?;
                        rlimit::setrlimit(
                            rlimit::Resource::CORE,
                            rlimit::INFINITY,
                            rlimit::INFINITY,
                        )?;
                        Some(limits)
                    } else {
                        None
                    };

                    #[cfg(not(unix))]
                    let _ = with_rlimit_unlimited_core_size;

                    let arg_string = || {
                        args.iter()
                            .map(|v| v.to_string_lossy().to_string())
                            .collect::<Vec<_>>()
                            .join(" ")
                    };

                    let env_string = with_env
                        .iter()
                        .map(|(k, v)| format!("{k}='{v}'"))
                        .collect::<Vec<_>>()
                        .join(" ");

                    // nextest has meaningful exit codes that we want to parse.
                    // <https://github.com/nextest-rs/nextest/blob/main/nextest-metadata/src/exit_codes.rs#L12>
                    //
                    // unfortunately, xshell doesn't have a mode where it can
                    // both emit to stdout/stderr, _and_ report the specific
                    // exit code of the process.
                    //
                    // So we have to use the raw process API instead.
                    log::info!(
                        "$ {} {} {}",
                        env_string,
                        argv0.to_string_lossy(),
                        arg_string()
                    );
                    let mut command = std::process::Command::new(&argv0);
                    command.args(&args).envs(with_env).current_dir(&working_dir);

                    let mut child = command.spawn().with_context(|| {
                        format!(
                            "failed to spawn '{} {}'",
                            argv0.to_string_lossy(),
                            arg_string()
                        )
                    })?;

                    let status = child.wait()?;

                    #[cfg(unix)]
                    if let Some((soft, hard)) = old_core_rlimits {
                        rlimit::setrlimit(rlimit::Resource::CORE, soft, hard)?;
                    }

                    let all_tests_passed = match (status.success(), status.code()) {
                        (true, _) => true,
                        // documented nextest exit code for when a test has failed
                        (false, Some(100)) => false,
                        // any other exit code means something has gone disastrously wrong
                        (false, _) => anyhow::bail!("failed to run nextest"),
                    };

                    rt.write(all_tests_passed_var, &all_tests_passed);

                    if !all_tests_passed {
                        log::warn!("encountered at least one test failure!");

                        if terminate_job_on_fail {
                            anyhow::bail!("terminating job (TerminateJobOnFail = true)")
                        } else {
                            // special string on ADO that causes step to show orange (!)
                            // FUTURE: flowey should prob have a built-in API for this
                            if matches!(rt.backend(), FlowBackend::Ado) {
                                eprintln!("##vso[task.complete result=SucceededWithIssues;]")
                            } else {
                                log::warn!("encountered at least one test failure");
                            }
                        }
                    }

                    let junit_xml = if let Some(junit_path) = junit_path {
                        let emitted_xml = working_dir
                            .join("target")
                            .join("nextest")
                            .join(&nextest_profile)
                            .join(junit_path);
                        let final_xml = std::env::current_dir()?.join("junit.xml");
                        // copy locally to avoid trashing the output between test runs
                        fs_err::rename(emitted_xml, &final_xml)?;
                        Some(final_xml.absolute()?)
                    } else {
                        None
                    };

                    rt.write(junit_xml_write, &junit_xml);

                    Ok(())
                }
            });

            ctx.emit_minor_rust_step("write results", |ctx| {
                let all_tests_passed = all_tests_passed_read.claim(ctx);
                let junit_xml = junit_xml_read.claim(ctx);
                let results = results.claim(ctx);

                move |rt| {
                    let all_tests_passed = rt.read(all_tests_passed);
                    let junit_xml = rt.read(junit_xml);

                    rt.write(
                        results,
                        &TestResults {
                            all_tests_passed,
                            junit_xml,
                        },
                    );
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
    features: build_params::FeatureSet,
    unstable_panic_abort_tests: Option<build_params::PanicAbortTests>,
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

    let features: Vec<String> = {
        let mut v = Vec::new();

        if no_default_features {
            v.push("--no-default-features".into())
        }

        match features {
            build_params::FeatureSet::All => v.push("--all-features".into()),
            build_params::FeatureSet::Specific(features) => {
                if !features.is_empty() {
                    v.push("--features".into());
                    v.push(features.join(","));
                }
            }
        }

        v
    };

    let (z_panic_abort_tests, use_rustc_bootstrap) = match unstable_panic_abort_tests {
        Some(kind) => (
            Some("-Zpanic-abort-tests"),
            match kind {
                build_params::PanicAbortTests::UsingNightly => false,
                build_params::PanicAbortTests::UsingRustcBootstrap => true,
            },
        ),
        None => (None, false),
    };

    let mut args = Vec::new();
    args.extend(locked.map(Into::into));
    args.extend(verbose.map(Into::into));
    args.push("--cargo-profile".into());
    args.push(cargo_profile.into());
    args.extend(z_panic_abort_tests.map(Into::into));
    args.push("--target".into());
    args.push(target);
    args.extend(packages);
    args.extend(features);

    let mut env = BTreeMap::new();
    if use_rustc_bootstrap {
        env.insert("RUSTC_BOOTSTRAP".into(), "1".into());
    }
    env.append(&mut extra_env);

    (args, env)
}

// FUTURE: this seems like something a proc-macro can help with...
impl build_params::NextestBuildParams {
    pub fn claim(self, ctx: &mut StepCtx<'_>) -> build_params::NextestBuildParams<VarClaimed> {
        let build_params::NextestBuildParams {
            packages,
            features,
            no_default_features,
            unstable_panic_abort_tests,
            target,
            profile,
            extra_env,
        } = self;

        build_params::NextestBuildParams {
            packages: packages.claim(ctx),
            features,
            no_default_features,
            unstable_panic_abort_tests,
            target,
            profile,
            extra_env: extra_env.claim(ctx),
        }
    }
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
            } => RunKindDeps::RunFromArchive {
                archive_file: archive_file.claim(ctx),
                nextest_bin: nextest_bin.claim(ctx),
            },
        }
    }
}
