// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use self::cargo_fuzz::CargoFuzzCommand;
use self::parse_fuzz_crate_toml::RepoFuzzTarget;
use crate::Xtask;
use anyhow::Context;
use clap::Parser;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

mod cargo_fuzz;
mod html_coverage;
mod init_from_template;
mod onefuzz_schema;
mod parse_fuzz_crate_toml;

/// Xtask to interact with with fuzzing infrastructure in the OpenVMM repo.
#[derive(Parser)]
#[clap(
    about = "Superset of `cargo fuzz` features, tailored to the OpenVMM repo",
    disable_help_subcommand = true
)]
#[clap(after_help = r#"ADDITIONAL NOTES:

    Fuzzers in the OpenVMM repo are required to include a
    [package.metadata.xtask.fuzz.onefuzz-allowlist] section in their Cargo.toml.

    Allowlists are used by OneFuzz to limit which files are considered when
    generating code coverage reports.

    A typical declaration might something like:

        [package.metadata.xtask.fuzz.onefuzz-allowlist]
        fuzz_my_crate = [
            "fuzz_my_crate.rs"
            "../src/**/*",
            "!../src/vendored/**/*"
        ]

    In this example, code coverage reports would consider the fuzzer itself
    and all files under the crate's `src/` directory, excluding code under
    `src/vendored/`.

    NOTE: Omitting this table will result in verification failures!
"#)]
pub struct Fuzz {
    /// Subcommand
    #[clap(subcommand)]
    cmd: FuzzCommand,
}

#[derive(clap::Subcommand)]
enum FuzzCommand {
    /// Onboard a new fuzz target corresponding to a particular crate
    Init {
        /// Crate name to spin up fuzzing infrastructure for
        package: String,

        /// Fuzzer template to init with
        template: init_from_template::Template,
    },
    /// List all available fuzz targets in the repo
    List {
        /// List all available fuzz *crates* in the repo.
        ///
        /// A fuzz crate can contain multiple fuzz targets.
        ///
        /// This option is mainly useful when running in CI, as it can be used
        /// to generate an "exclude list" of fuzz crates, which we skip building
        /// on platforms that `libfuzzer-sys` doesn't support (notably: musl).
        #[clap(long)]
        crates: bool,
    },
    /// Verify the `Cargo.toml` and directory structures of in-tree fuzzing
    /// crates.
    Verify,
    /// Build fuzz targets.
    Build {
        /// Fuzzing targets to build. If not specified, builds all available
        /// targets.
        targets: Vec<String>,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,

        /// Extra args to forward to each `cargo fuzz build` invocation.
        #[clap(raw(true))]
        extra: Vec<String>,
    },
    /// Run fuzz targets.
    Run {
        /// Fuzzing target to run.
        target: String,

        /// Path to specific repro case
        artifact: Option<PathBuf>,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,

        /// Extra args to forward to `cargo fuzz run`.
        #[clap(raw(true))]
        extra: Vec<String>,
    },
    /// Clean local fuzzing artifacts/corpus/coverage
    Clean {
        /// Fuzzing targets to clean. If not specified, cleans all available
        /// targets.
        targets: Vec<String>,

        /// Don't delete corpus directories
        #[clap(long)]
        keep_corpus: bool,

        /// Don't delete artifact directories
        #[clap(long)]
        keep_artifacts: bool,

        /// Don't delete coverage directories
        #[clap(long)]
        keep_coverage: bool,
    },
    /// Print the `std::fmt::Debug` output for an input.
    Fmt {
        /// Fuzzing target the input corresponds to.
        target: String,

        /// Path to input file.
        input: PathBuf,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,

        /// Extra args to forward to `cargo fuzz fmt`.
        #[clap(raw(true))]
        extra: Vec<String>,
    },
    /// Minify a corpus.
    Cmin {
        /// Fuzzing target to minify the corpus of.
        target: String,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,

        /// Extra args to forward to `cargo fuzz cmin`.
        #[clap(raw(true))]
        extra: Vec<String>,
    },
    /// Minify a test case.
    Tmin {
        /// Fuzzing target the test case corresponds to.
        target: String,

        /// Path to test case file.
        test_case: PathBuf,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,

        /// Extra args to forward to `cargo fuzz tmin`.
        #[clap(raw(true))]
        extra: Vec<String>,
    },
    /// Run program on the generated corpus and generate coverage information.
    Coverage {
        /// Fuzzing target the corpus corresponds to.
        target: String,

        /// Also generate an HTML coverage report using `lcov` and `genhtml`.
        #[clap(long)]
        with_html_report: bool,

        /// Skip rebuilding + collecting new coverage data.
        #[clap(long, requires = "with_html_report")]
        only_report: bool,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,

        /// Extra args to forward to `cargo fuzz coverage`.
        #[clap(raw(true))]
        extra: Vec<String>,
    },
    /// Build fuzzers and construct a Onefuzz-ready drop folder.
    Onefuzz {
        /// Path to the OneFuzz configuration file.
        config_path: PathBuf,

        /// Output directory to emit files to.
        out_dir: PathBuf,

        /// Specific targets to include. If left blank, includes all available
        /// targets.
        target: Vec<String>,

        /// The Rust toolchain to use. Defaults to the environment's default toolchain.
        #[clap(long)]
        toolchain: Option<String>,
    },
    /// (debug) Dump raw debug info about all available fuzz targets
    Dump,
}

mod cargo_package_metadata {
    use serde::Deserialize;
    use serde::Serialize;
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    pub struct PackageMetadata {
        #[serde(rename = "cargo-fuzz")]
        pub cargo_fuzz: Option<bool>, // piggyback off cargo-fuzz infra
        pub xtask: Option<Xtask>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Xtask {
        pub fuzz: Option<Fuzz>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Fuzz {
        #[serde(rename = "onefuzz-allowlist")]
        pub allowlist: BTreeMap<String, Vec<String>>,

        #[serde(default, rename = "target-options")]
        pub target_options: BTreeMap<String, Vec<String>>,
    }
}

impl Xtask for Fuzz {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let fuzz_crates = parse_fuzz_crate_toml::get_repo_fuzz_crates(&ctx)?;
        let fuzz_targets = parse_fuzz_crate_toml::get_repo_fuzz_targets(&fuzz_crates)?;

        match self.cmd {
            FuzzCommand::Init { package, template } => {
                init_from_template::init_from_template(&ctx, package, template)?;
            }
            FuzzCommand::Dump => {
                println!("{:#?}", fuzz_targets)
            }
            FuzzCommand::Verify => {
                // essentially a no-op, since if we made it this far, that means
                // that everything validated correctly.
                log::info!("fuzzing crates were successfully verified!")
            }
            FuzzCommand::List { crates } => {
                if crates {
                    for parse_fuzz_crate_toml::FuzzCrateMetadata { crate_name, .. } in fuzz_crates {
                        println!("{}", crate_name)
                    }
                } else {
                    for (name, _meta) in fuzz_targets {
                        println!("{}", name)
                    }
                }
            }
            FuzzCommand::Build {
                targets,
                toolchain,
                extra,
            } => {
                let selected_fuzz_targets = filter_fuzz_targets(fuzz_targets, targets)?;

                for (name, meta) in selected_fuzz_targets {
                    println!("building '{}'", name);
                    CargoFuzzCommand::Build.invoke(
                        &name,
                        &meta.fuzz_dir,
                        &meta.target_options,
                        toolchain.as_deref(),
                        &extra,
                    )?;
                }
            }
            FuzzCommand::Run {
                target: target_name,
                artifact,
                toolchain,
                extra,
            } => {
                let target = select_fuzz_target(fuzz_targets, &target_name)?;
                let res = CargoFuzzCommand::Run { artifact }.invoke(
                    &target_name,
                    &target.fuzz_dir,
                    &target.target_options,
                    toolchain.as_deref(),
                    &extra,
                );

                if let Err(e) = res {
                    log::warn!(
                        "Reminder: Make sure you swap `cargo fuzz` with `cargo xtask fuzz` when repro-ing / minimizing failures in the OpenVMM repo!"
                    );
                    return Err(e);
                }
            }
            FuzzCommand::Fmt {
                target: target_name,
                input,
                extra,
                toolchain,
            } => {
                let target = select_fuzz_target(fuzz_targets, &target_name)?;

                CargoFuzzCommand::Fmt { input }.invoke(
                    &target_name,
                    &target.fuzz_dir,
                    &target.target_options,
                    toolchain.as_deref(),
                    &extra,
                )?;
            }
            FuzzCommand::Cmin {
                target: target_name,
                extra,
                toolchain,
            } => {
                let target = select_fuzz_target(fuzz_targets, &target_name)?;

                CargoFuzzCommand::Cmin.invoke(
                    &target_name,
                    &target.fuzz_dir,
                    &target.target_options,
                    toolchain.as_deref(),
                    &extra,
                )?;
            }
            FuzzCommand::Tmin {
                target: target_name,
                test_case,
                toolchain,
                extra,
            } => {
                let target = select_fuzz_target(fuzz_targets, &target_name)?;
                let res = CargoFuzzCommand::Tmin { test_case }.invoke(
                    &target_name,
                    &target.fuzz_dir,
                    &target.target_options,
                    toolchain.as_deref(),
                    &extra,
                );

                if let Err(e) = res {
                    log::warn!(
                        "Reminder: Make sure you swap `cargo fuzz` with `cargo xtask fuzz` when repro-ing / minimizing failures in the OpenVMM repo!"
                    );
                    return Err(e);
                }
            }
            FuzzCommand::Coverage {
                target: target_name,
                with_html_report,
                only_report,
                toolchain,
                extra,
            } => {
                let target = select_fuzz_target(fuzz_targets, &target_name)?;

                if !only_report {
                    CargoFuzzCommand::Coverage.invoke(
                        &target_name,
                        &target.fuzz_dir,
                        &target.target_options,
                        toolchain.as_deref(),
                        &extra,
                    )?;
                }

                if with_html_report {
                    html_coverage::generate_html_coverage_report(
                        &ctx,
                        &target.fuzz_dir,
                        &target_name,
                    )?;
                }
            }
            FuzzCommand::Onefuzz {
                target,
                config_path,
                toolchain,
                out_dir,
            } => {
                let selected_fuzz_targets = filter_fuzz_targets(fuzz_targets, target)?;

                if !out_dir.exists() {
                    fs_err::create_dir_all(&out_dir)?;
                }

                let config_contents = fs_err::read_to_string(config_path)
                    .context("failed to read configuration toml")?;
                let cfg = toml_edit::de::from_str(&config_contents)
                    .context("failed to parse onefuzz.toml")?;

                for (name, target) in &selected_fuzz_targets {
                    log::info!("building '{}'", name);
                    CargoFuzzCommand::Build.invoke(
                        name,
                        &target.fuzz_dir,
                        &target.target_options,
                        toolchain.as_deref(),
                        &[],
                    )?;

                    log::info!("copying '{}' to output folder", name);
                    // Because we call Build ourselves above we guarantee that
                    // the built binaries are here.
                    std::fs::copy(
                        format!("target/x86_64-unknown-linux-gnu/release/{}", name),
                        out_dir.join(name),
                    )?;

                    log::info!("emitting onefuzz allowlist for '{name}'");
                    let mut allowlist_file =
                        fs_err::File::create(out_dir.join(name).with_extension("txt"))?;
                    for path in &target.allowlist {
                        let Ok(path) = path.strip_prefix(&ctx.root) else {
                            // Ok to throw away `std::path::StripPrefixError`,
                            // it doesn't contain any additional context
                            anyhow::bail!(
                                "allowlist for '{name}' references file(s) outside of the OpenVMM directory"
                            )
                        };
                        // add in "*/" to appease the OneFuzz allowlist syntax
                        writeln!(allowlist_file, "*/{}", path.display())?;
                    }
                }

                log::info!("emitting OneFuzzConfig.json");
                let config_file =
                    fs_err::File::create(out_dir.join("OneFuzzConfig").with_extension("json"))?;
                let config = onefuzz_schema::OneFuzzConfigV3 {
                    config_version: 3,
                    entries: selected_fuzz_targets
                        .into_iter()
                        .map(|(name, target)| make_onefuzz_entry(name, target.target_options, &cfg))
                        .collect(),
                };
                serde_json::to_writer(config_file, &config)?;
            }
            FuzzCommand::Clean {
                targets,
                keep_corpus,
                keep_artifacts,
                keep_coverage,
            } => {
                let selected_fuzz_targets = filter_fuzz_targets(fuzz_targets, targets)?;

                for (name, meta) in selected_fuzz_targets {
                    let rm_dir = |base: &str| -> std::io::Result<()> {
                        let dir = meta.fuzz_dir.join(base);
                        let target_dir = dir.join(&name);
                        if target_dir.exists() {
                            fs_err::remove_dir_all(dir.join(&name))?;
                        }
                        if dir.exists() && fs_err::read_dir(&dir)?.count() == 0 {
                            fs_err::remove_dir(dir)?;
                        }

                        Ok(())
                    };

                    if !keep_artifacts {
                        rm_dir("artifacts")?
                    }

                    if !keep_corpus {
                        rm_dir("corpus")?
                    }

                    if !keep_coverage {
                        rm_dir("coverage")?
                    }
                }
            }
        }

        Ok(())
    }
}

fn make_onefuzz_entry(
    name: String,
    target_options: Vec<String>,
    cfg: &OnefuzzToml,
) -> onefuzz_schema::Entry {
    let my_cfg = cfg.overrides.get(&name);
    let use_cfg = OnefuzzTomlConfig {
        owner: my_cfg
            .and_then(|m| m.owner.clone())
            .unwrap_or(cfg.default.owner.clone()),
        project_name: my_cfg
            .and_then(|m| m.project_name.clone())
            .unwrap_or(cfg.default.project_name.clone()),
        ado_org: my_cfg
            .and_then(|m| m.ado_org.clone())
            .unwrap_or(cfg.default.ado_org.clone()),
        ado_project: my_cfg
            .and_then(|m| m.ado_project.clone())
            .unwrap_or(cfg.default.ado_project.clone()),
        ado_assigned_to: my_cfg
            .and_then(|m| m.ado_assigned_to.clone())
            .unwrap_or(cfg.default.ado_assigned_to.clone()),
        ado_area_path: my_cfg
            .and_then(|m| m.ado_area_path.clone())
            .unwrap_or(cfg.default.ado_area_path.clone()),
        ado_iteration_path: my_cfg
            .and_then(|m| m.ado_iteration_path.clone())
            .unwrap_or(cfg.default.ado_iteration_path.clone()),
        ado_tags: my_cfg
            .and_then(|m| m.ado_tags.clone())
            .unwrap_or(cfg.default.ado_tags.clone()),
    };

    onefuzz_schema::Entry {
        job_notification_email: use_cfg.owner,
        fuzzer: onefuzz_schema::Fuzzer {
            type_field: "libfuzzer".to_owned(),
            fuzzing_harness_executable_name: name.clone(),
            sources_allow_list_path: format!("{}.txt", name),
        },
        job_dependencies: vec![name.clone()],
        one_fuzz_jobs: vec![onefuzz_schema::OneFuzzJob {
            project_name: use_cfg.project_name.clone(),
            target_name: name.clone(),
            target_options,
        }],
        ado_template: onefuzz_schema::AdoTemplate {
            org: use_cfg.ado_org,
            project: use_cfg.ado_project,
            assigned_to: use_cfg.ado_assigned_to,
            area_path: use_cfg.ado_area_path,
            iteration_path: use_cfg.ado_iteration_path,
            ado_fields: onefuzz_schema::AdoFields {
                tags: use_cfg.ado_tags,
            },
        },
    }
}

/// Check that all fuzz targets in the repo follow the correct formatting.
// DEVNOTE: used by `xtask fmt`
#[derive(Parser)]
pub struct VerifyFuzzers;

impl Xtask for VerifyFuzzers {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let fuzz_crates = parse_fuzz_crate_toml::get_repo_fuzz_crates(&ctx)?;
        let _fuzz_targets = parse_fuzz_crate_toml::get_repo_fuzz_targets(&fuzz_crates)?;
        Ok(())
    }
}

fn select_fuzz_target(
    mut fuzz_targets: BTreeMap<String, RepoFuzzTarget>,
    target_name: &str,
) -> anyhow::Result<RepoFuzzTarget> {
    match fuzz_targets.remove(target_name) {
        Some(target) => Ok(target),
        None => anyhow::bail!("invalid fuzz target '{}'", target_name),
    }
}

fn filter_fuzz_targets(
    mut fuzz_targets: BTreeMap<String, RepoFuzzTarget>,
    specific_targets: Vec<String>,
) -> anyhow::Result<BTreeMap<String, RepoFuzzTarget>> {
    if specific_targets.is_empty() {
        return Ok(fuzz_targets);
    }
    let mut targets = BTreeMap::new();
    for target_name in specific_targets {
        let Some(target) = fuzz_targets.remove(&target_name) else {
            anyhow::bail!("invalid fuzz target '{}'", target_name)
        };

        targets.insert(target_name, target);
    }

    Ok(targets)
}

pub(crate) fn complete_fuzzer_targets(ctx: &crate::XtaskCtx) -> Vec<String> {
    (|| {
        let fuzz_crates = parse_fuzz_crate_toml::get_repo_fuzz_crates(ctx)?;
        let fuzz_targets = parse_fuzz_crate_toml::get_repo_fuzz_targets(&fuzz_crates)?;
        anyhow::Ok(fuzz_targets.into_keys().collect::<Vec<String>>())
    })()
    .unwrap_or_default()
}

#[derive(Deserialize)]
struct OnefuzzToml {
    default: OnefuzzTomlConfig,
    overrides: BTreeMap<String, OnefuzzTomlOverrides>,
}

#[derive(Deserialize)]
struct OnefuzzTomlConfig {
    owner: String,
    project_name: String,
    ado_org: String,
    ado_project: String,
    ado_assigned_to: String,
    ado_area_path: String,
    ado_iteration_path: String,
    ado_tags: String,
}

#[derive(Deserialize)]
struct OnefuzzTomlOverrides {
    owner: Option<String>,
    project_name: Option<String>,
    ado_org: Option<String>,
    ado_project: Option<String>,
    ado_assigned_to: Option<String>,
    ado_area_path: Option<String>,
    ado_iteration_path: Option<String>,
    ado_tags: Option<String>,
}
