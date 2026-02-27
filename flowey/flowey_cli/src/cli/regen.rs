// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

/// Regenerate all pipelines defined in the repo's root `.flowey.toml`
#[derive(clap::Args)]
pub struct Regen {
    /// Check that pipelines are up to date, without regenerating them.
    #[clap(long)]
    check: bool,

    /// Pass `--quiet` to any subprocess invocations of `cargo run`.
    #[clap(long)]
    quiet: bool,
}

impl Regen {
    pub fn run(self, repo_root: &Path) -> anyhow::Result<()> {
        install_flowey_merge_driver()?;

        if !repo_root.join(".flowey.toml").exists() {
            log::warn!("no .flowey.toml exists in the repo root");
            return Ok(());
        }

        let flowey_toml = fs_err::read_to_string(repo_root.join(".flowey.toml"))?;
        let flowey_toml: flowey_toml::FloweyToml =
            toml_edit::de::from_str(&flowey_toml).context("while parsing .flowey.toml")?;

        let data = resolve_flowey_toml(flowey_toml, repo_root.to_owned())
            .context("while resolving .flowey.toml")?;

        let mut bin2flowey = BTreeMap::<String, PathBuf>::new();

        let mut error = false;
        for ResolvedFloweyToml {
            working_dir,
            pipelines,
        } in data
        {
            for (bin_name, pipelines) in pipelines {
                let exe_name = format!("{bin_name}{}", std::env::consts::EXE_SUFFIX);

                let bin = if let Some(bin) = bin2flowey.get(&bin_name) {
                    bin.clone()
                } else {
                    // build the requested flowey
                    {
                        let quiet = self.quiet.then_some("-q");
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "not in a flowey runtime context"
                        )]
                        let sh = xshell::Shell::new()?;
                        sh.change_dir(&working_dir);
                        #[expect(clippy::disallowed_macros)]
                        xshell::cmd!(sh, "cargo build -p {bin_name} {quiet...}").run()?;
                    }

                    // find the built flowey
                    let bin = working_dir
                        .join(
                            std::env::var("CARGO_TARGET_DIR")
                                .as_deref()
                                .unwrap_or("target"),
                        )
                        .join(std::env::var("CARGO_BUILD_TARGET").as_deref().unwrap_or(""))
                        .join("debug")
                        .join(&exe_name);

                    if !bin.exists() {
                        panic!("should have found built {bin_name} at {}", bin.display());
                    }

                    // stash result for future consumers
                    bin2flowey.insert(bin_name.clone(), bin.clone());
                    bin
                };

                for (backend, defns) in pipelines {
                    for flowey_toml::PipelineDefn { file, cmd } in defns {
                        let check = if self.check {
                            vec!["--check".into(), file.display().to_string()]
                        } else {
                            vec![]
                        };

                        #[expect(
                            clippy::disallowed_methods,
                            reason = "not in a flowey runtime context"
                        )]
                        let sh = xshell::Shell::new()?;
                        sh.change_dir(&working_dir);
                        #[expect(clippy::disallowed_macros)]
                        let res = xshell::cmd!(
                            sh,
                            "{bin} pipeline {backend} --out {file} {check...} {cmd...}"
                        )
                        .run();

                        if res.is_err() {
                            error = true;
                        }
                    }
                }
            }
        }

        if error {
            anyhow::bail!("encountered one or more errors")
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct ResolvedFloweyToml {
    pub working_dir: PathBuf,
    // (bin, (backend, metadata))
    pub pipelines: BTreeMap<String, BTreeMap<String, Vec<flowey_toml::PipelineDefn>>>,
}

fn resolve_flowey_toml(
    flowey_toml: flowey_toml::FloweyToml,
    working_dir: PathBuf,
) -> anyhow::Result<Vec<ResolvedFloweyToml>> {
    let mut v = Vec::new();
    resolve_flowey_toml_inner(flowey_toml, working_dir, &mut v)?;
    Ok(v)
}

fn resolve_flowey_toml_inner(
    flowey_toml: flowey_toml::FloweyToml,
    working_dir: PathBuf,
    resolved: &mut Vec<ResolvedFloweyToml>,
) -> anyhow::Result<()> {
    let flowey_toml::FloweyToml { include, pipeline } = flowey_toml;

    let mut resolved_pipelines: BTreeMap<String, BTreeMap<String, Vec<_>>> = BTreeMap::new();
    for (bin_name, pipelines) in pipeline {
        for (backend, defns) in pipelines {
            resolved_pipelines
                .entry(bin_name.clone())
                .or_default()
                .entry(backend)
                .or_default()
                .extend(defns);
        }
    }

    for path in include.unwrap_or_default() {
        let path = working_dir.join(path);
        let flowey_toml = fs_err::read_to_string(&path)?;
        let flowey_toml: flowey_toml::FloweyToml = toml_edit::de::from_str(&flowey_toml)
            .with_context(|| anyhow::anyhow!("while parsing {}", path.display()))?;
        let mut working_dir = path;
        working_dir.pop();
        resolve_flowey_toml_inner(flowey_toml, working_dir, resolved)?
    }

    resolved.push(ResolvedFloweyToml {
        working_dir,
        pipelines: resolved_pipelines,
    });

    Ok(())
}

mod flowey_toml {
    use serde::Deserialize;
    use serde::Serialize;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct FloweyToml {
        pub include: Option<Vec<PathBuf>>,
        // (bin, (backend, metadata))
        pub pipeline: BTreeMap<String, BTreeMap<String, Vec<PipelineDefn>>>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct PipelineDefn {
        pub file: PathBuf,
        pub cmd: Vec<String>,
    }
}

fn install_flowey_merge_driver() -> anyhow::Result<()> {
    const DRIVER_NAME: &str = "flowey-theirs merge driver";
    const DRIVER_COMMAND: &str = "cp %B %A";

    #[expect(clippy::disallowed_methods, reason = "not in a flowey runtime context")]
    let sh = xshell::Shell::new()?;
    #[expect(clippy::disallowed_macros, reason = "not in a flowey runtime context")]
    xshell::cmd!(sh, "git config merge.flowey-theirs.name {DRIVER_NAME}")
        .quiet()
        .ignore_status()
        .run()?;
    #[expect(clippy::disallowed_macros, reason = "not in a flowey runtime context")]
    xshell::cmd!(sh, "git config merge.flowey-theirs.driver {DRIVER_COMMAND}")
        .quiet()
        .ignore_status()
        .run()?;

    Ok(())
}
