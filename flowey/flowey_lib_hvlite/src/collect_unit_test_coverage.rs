// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Collect local, per-test LLVM coverage using the unit-test build plan.

use flowey::node::prelude::*;
use flowey_lib_common::run_cargo_nextest_run::build_params::NextestBuildParams;

#[derive(Serialize, Deserialize)]
pub struct Options {
    pub output: PathBuf,
    pub filter: Option<String>,
    pub base: Option<String>,
}

flowey_request! {
    pub enum Request {
        Begin {
            repo_root: PathBuf,
            done: WriteVar<SideEffect>,
        },
        Collect {
            xtask: ReadVar<crate::build_xtask::XtaskOutput>,
            runs: Vec<(String, NextestBuildParams)>,
            options: Options,
            working_dir: ReadVar<PathBuf>,
            pre_run_deps: Vec<ReadVar<SideEffect>>,
            done: WriteVar<SideEffect>,
        },
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::cfg_cargo_common_flags::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
        ctx.import::<flowey_lib_common::install_cargo_nextest::Node>();
        ctx.import::<flowey_lib_common::install_cargo_llvm_cov::Node>();
    }

    fn emit(requests: Vec<Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(
            matches!(ctx.backend(), FlowBackend::Local),
            "unit coverage currently supports only the local backend"
        );
        for request in requests {
            let (xtask, runs, options, working_dir, pre_run_deps, done) = match request {
                Request::Begin { repo_root, done } => {
                    ctx.emit_rust_step("clear active unit coverage", |ctx| {
                        done.claim(ctx);
                        move |_rt| match fs_err::remove_file(
                            repo_root.join("target/unit-coverage/latest.json"),
                        ) {
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                            result => result.context("clearing previous active unit coverage"),
                        }
                    });
                    continue;
                }
                Request::Collect {
                    xtask,
                    runs,
                    options,
                    working_dir,
                    pre_run_deps,
                    done,
                } => (xtask, runs, options, working_dir, pre_run_deps, done),
            };
            let flags = ctx.reqv(flowey_lib_common::cfg_cargo_common_flags::Request::GetFlags);
            ctx.req(flowey_lib_common::install_rust::Request::InstallComponent(
                format!("llvm-tools-{}", target_lexicon::Triple::host(),),
            ));
            let toolchain = ctx.reqv(flowey_lib_common::install_rust::Request::GetRustupToolchain);
            let nextest = ctx.reqv(flowey_lib_common::install_cargo_nextest::Request);
            let llvm_cov = ctx.reqv(flowey_lib_common::install_cargo_llvm_cov::Request);
            ctx.emit_rust_step("collect local unit test coverage", |ctx| {
                done.claim(ctx);
                pre_run_deps.claim(ctx);
                nextest.claim(ctx);
                llvm_cov.claim(ctx);
                let flags = flags.claim(ctx);
                let toolchain = toolchain.claim(ctx);
                let working_dir = working_dir.claim(ctx);
                let xtask = xtask.claim(ctx);
                let runs: Vec<_> = runs
                    .into_iter()
                    .map(|(name, params)| (name, params.claim(ctx)))
                    .collect();
                move |rt| {
                    let working_dir = rt.read(working_dir);
                    let xtask = match rt.read(xtask) {
                        crate::build_xtask::XtaskOutput::LinuxBin { bin, .. } => bin,
                        crate::build_xtask::XtaskOutput::WindowsBin { exe, .. } => exe,
                    };
                    let toolchain = rt.read(toolchain);
                    let script = working_dir.join("repo_support/unit_test_coverage.py");
                    let mut config = fs_err::read_to_string(
                        working_dir.join(".config/nextest.toml"),
                    )?.parse::<toml_edit::DocumentMut>()?;
                    if let Some(profiles) = config.get("profile").and_then(|v| v.as_table()) {
                        for (_, profile) in profiles {
                            if let Some(scripts) = profile
                                .get("scripts")
                                .and_then(|v| v.as_array_of_tables())
                            {
                                anyhow::ensure!(
                                    scripts.iter().all(|s| !s.contains_key("run-wrapper")),
                                    "unit coverage cannot replace an existing nextest run-wrapper"
                                );
                            }
                        }
                    }
                    let experimental = config
                        .entry("experimental")
                        .or_insert(toml_edit::value(toml_edit::Array::new()))
                        .as_array_mut()
                        .context("nextest experimental features must be an array")?;
                    if !experimental.iter().any(|v| v.as_str() == Some("wrapper-scripts")) {
                        experimental.push("wrapper-scripts");
                    }
                    let mut command = toml_edit::Array::new();
                    for arg in ["python3", &script.display().to_string(), "nextest-runner"] {
                        command.push(arg);
                    }
                    config["scripts"]["wrapper"]["unit-coverage"]["command"] =
                        toml_edit::value(command);
                    config["scripts"]["wrapper"]["unit-coverage"]["target-runner"] =
                        toml_edit::value("within-wrapper");
                    let mut rule = toml_edit::Table::new();
                    rule["filter"] = toml_edit::value("all()");
                    rule["run-wrapper"] = toml_edit::value("unit-coverage");
                    config["profile"]["agent"]
                        .as_table_mut()
                        .context("missing nextest agent profile")?
                        .entry("scripts")
                        .or_insert(toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()))
                        .as_array_of_tables_mut()
                        .context("nextest scripts must be an array of tables")?
                        .push(rule);
                    let config_file = rt.sh.current_dir().join("unit-coverage-nextest.toml");
                    fs_err::write(&config_file, config.to_string())?;
                    let mut plan = Vec::new();
                    for (name, params) in runs {
                        let (args, env) =
                            flowey_lib_common::gen_cargo_nextest_run_cmd::cargo_nextest_build_args_and_env(
                                rt.read(flags.clone()),
                                params.profile,
                                params.target,
                                rt.read(params.packages),
                                params.features,
                                params.no_default_features,
                                rt.read(params.extra_env),
                            );
                        plan.push(serde_json::json!({ "name": name, "args": args, "env": env }));
                    }
                    let plan_file = rt.sh.current_dir().join("unit-coverage-plan.json");
                    fs_err::write(
                        &plan_file,
                        serde_json::to_vec(&serde_json::json!({
                            "runs": plan,
                            "toolchain": toolchain,
                            "filter": options.filter,
                            "nextest_config": config_file,
                            "base": options.base,
                            "xtask": xtask,
                        }))?,
                    )?;
                    let output = options.output;
                    rt.sh.change_dir(working_dir);
                    flowey::shell_cmd!(
                        rt,
                        "python3 {script} collect --plan {plan_file} --output {output}"
                    )
                    .run()
                    .context("unit coverage failed; see report.json for partial results if available")?;
                    Ok(())
                }
            });
        }
        Ok(())
    }
}
