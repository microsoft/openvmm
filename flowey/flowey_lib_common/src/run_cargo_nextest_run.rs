// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run cargo-nextest tests.

use crate::gen_cargo_nextest_run_cmd::RunKindDeps;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
pub struct TestResults {
    pub all_tests_passed: bool,
    /// Path to JUnit XML output (if enabled by the nextest profile)
    pub junit_xml: Option<PathBuf>,
}

/// Parameters related to building nextest tests
pub mod build_params {
    use crate::run_cargo_build::CargoBuildProfile;
    use crate::run_cargo_build::CargoFeatureSet;
    use flowey::node::prelude::*;
    use std::collections::BTreeMap;

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
        pub features: CargoFeatureSet,
        /// Whether to disable default features
        pub no_default_features: bool,
        /// Build tests for the specified target
        pub target: target_lexicon::Triple,
        /// Build tests with the specified cargo profile
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
    RunFromArchive {
        archive_file: ReadVar<PathBuf>,
        target: Option<ReadVar<target_lexicon::Triple>>,
        nextest_bin: Option<ReadVar<PathBuf>>,
    },
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
    /// Nextest partition for sharding tests across multiple CI jobs.
    pub nextest_partition: Option<crate::gen_cargo_nextest_run_cmd::NextestPartition>,
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

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::cfg_cargo_common_flags::Node>();
        ctx.import::<crate::download_cargo_nextest::Node>();
        ctx.import::<crate::install_cargo_nextest::Node>();
        ctx.import::<crate::install_rust::Node>();
        ctx.import::<crate::gen_cargo_nextest_run_cmd::Node>();
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
            nextest_partition,
            run_ignored,
            pre_run_deps,
            results,
        } in run
        {
            let run_kind_deps = match run_kind {
                NextestRunKind::BuildAndRun(params) => {
                    let cargo_flags = ctx.reqv(crate::cfg_cargo_common_flags::Request::GetFlags);

                    let nextest_installed = ctx.reqv(crate::install_cargo_nextest::Request);

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
                NextestRunKind::RunFromArchive {
                    archive_file,
                    target,
                    nextest_bin,
                } => {
                    let target =
                        target.unwrap_or(ReadVar::from_static(target_lexicon::Triple::host()));

                    let nextest_bin = nextest_bin.unwrap_or_else(|| {
                        ctx.reqv(|v| crate::download_cargo_nextest::Request::Get(target.clone(), v))
                    });

                    RunKindDeps::RunFromArchive {
                        archive_file,
                        nextest_bin,
                        target,
                    }
                }
            };

            let cmd = ctx.reqv(|v| crate::gen_cargo_nextest_run_cmd::Request {
                run_kind_deps,
                working_dir: working_dir.clone(),
                config_file: config_file.clone(),
                tool_config_files,
                nextest_profile: nextest_profile.clone(),
                nextest_filter_expr,
                run_ignored,
                fail_fast,
                nextest_partition,
                extra_env,
                extra_commands: None,
                portable: false,
                command: v,
            });

            let (all_tests_passed_read, all_tests_passed_write) = ctx.new_var();
            let (junit_xml_read, junit_xml_write) = ctx.new_var();

            ctx.emit_rust_step(format!("run '{friendly_name}' nextest tests"), |ctx| {
                pre_run_deps.claim(ctx);

                let working_dir = working_dir.claim(ctx);
                let config_file = config_file.claim(ctx);
                let all_tests_passed_var = all_tests_passed_write.claim(ctx);
                let junit_xml_write = junit_xml_write.claim(ctx);
                let cmd = cmd.claim(ctx);

                move |rt| {
                    let working_dir = rt.read(working_dir);
                    let config_file = rt.read(config_file);
                    let cmd = rt.read(cmd);

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

                    log::info!("{cmd}");

                    // nextest has meaningful exit codes that we want to parse.
                    // <https://github.com/nextest-rs/nextest/blob/main/nextest-metadata/src/exit_codes.rs#L12>
                    //
                    // unfortunately, xshell doesn't have a mode where it can
                    // both emit to stdout/stderr, _and_ report the specific
                    // exit code of the process.
                    //
                    // So we have to use the raw process API instead.
                    assert_eq!(cmd.commands.len(), 1);
                    let mut command = std::process::Command::new(&cmd.commands[0].0);
                    command
                        .args(&cmd.commands[0].1)
                        .envs(&cmd.env)
                        .current_dir(&working_dir);

                    let mut child = command.spawn().with_context(|| {
                        format!("failed to spawn '{}'", &cmd.commands[0].0.to_string_lossy())
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

// FUTURE: this seems like something a proc-macro can help with...
impl build_params::NextestBuildParams {
    pub fn claim(self, ctx: &mut StepCtx<'_>) -> build_params::NextestBuildParams<VarClaimed> {
        let build_params::NextestBuildParams {
            packages,
            features,
            no_default_features,
            target,
            profile,
            extra_env,
        } = self;

        build_params::NextestBuildParams {
            packages: packages.claim(ctx),
            features,
            no_default_features,
            target,
            profile,
            extra_env: extra_env.claim(ctx),
        }
    }
}
