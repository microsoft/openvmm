// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::exec_snippet::FloweyPipelineStaticDb;
use super::exec_snippet::VarDbBackendKind;
use anyhow::Context;
use clap::ValueEnum;
use flowey_core::node::RuntimeVarDb;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

pub struct VarDbRequest<'a> {
    flowey_bin: &'a str,
    job_idx: usize,
    var_name: &'a str,
    action: RequestAction<'a>,
    is_raw_string: bool,
    condvar: Option<&'a str>,
}

enum RequestAction<'a> {
    WriteToEnv {
        backend: EnvBackend,
        env: &'a str,
    },
    Update {
        file: Option<&'a Path>,
        is_secret: bool,
        env_source: Option<&'a str>,
    },
}

pub struct VarDbRequestBuilder<'a> {
    flowey_bin: &'a str,
    job_idx: usize,
}

impl<'a> VarDbRequestBuilder<'a> {
    pub fn new(flowey_bin: &'a str, job_idx: usize) -> Self {
        Self {
            flowey_bin,
            job_idx,
        }
    }

    fn req<'b>(&'b self, var_name: &'b str, action: RequestAction<'b>) -> VarDbRequest<'b> {
        VarDbRequest::new(self.flowey_bin, self.job_idx, var_name, action)
    }

    pub fn write_to_ado_env<'b>(&'b self, var_name: &'b str, env: &'b str) -> VarDbRequest<'b> {
        self.req(
            var_name,
            RequestAction::WriteToEnv {
                backend: EnvBackend::Ado,
                env,
            },
        )
    }

    pub fn write_to_gh_env<'b>(&'b self, var_name: &'b str, env: &'b str) -> VarDbRequest<'b> {
        self.req(
            var_name,
            RequestAction::WriteToEnv {
                backend: EnvBackend::Github,
                env,
            },
        )
    }

    pub fn update_from_stdin<'b>(&'b self, var_name: &'b str, is_secret: bool) -> VarDbRequest<'b> {
        self.req(
            var_name,
            RequestAction::Update {
                file: None,
                is_secret,
                env_source: None,
            },
        )
    }

    #[expect(dead_code)]
    pub fn update_from_file<'b>(
        &'b self,
        var_name: &'b str,
        file: &'b Path,
        is_secret: bool,
    ) -> VarDbRequest<'b> {
        self.req(
            var_name,
            RequestAction::Update {
                file: Some(file),
                is_secret,
                env_source: None,
            },
        )
    }
}

impl<'a> VarDbRequest<'a> {
    fn new(
        flowey_bin: &'a str,
        job_idx: usize,
        var_name: &'a str,
        action: RequestAction<'a>,
    ) -> Self {
        Self {
            flowey_bin,
            job_idx,
            var_name,
            action,
            is_raw_string: false,
            condvar: None,
        }
    }

    pub fn raw_string(self, is_raw_string: bool) -> Self {
        Self {
            is_raw_string,
            ..self
        }
    }

    pub fn condvar(self, condvar: Option<&'a str>) -> Self {
        Self { condvar, ..self }
    }

    #[track_caller]
    pub fn env_source(mut self, source: Option<&'a str>) -> Self {
        let RequestAction::Update { env_source, .. } = &mut self.action else {
            panic!("env_source can only be set on Update actions");
        };
        *env_source = source;
        self
    }
}

impl std::fmt::Display for VarDbRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            flowey_bin,
            job_idx,
            var_name,
            ref action,
            is_raw_string,
            condvar,
        } = *self;

        write!(f, r#"{flowey_bin} v {job_idx} '{var_name}'"#)?;

        if is_raw_string {
            f.write_str(" --is-raw-string")?;
        }

        if let Some(condvar) = condvar {
            write!(f, " --condvar {condvar}")?;
        }

        match *action {
            RequestAction::WriteToEnv { backend, env } => {
                write!(
                    f,
                    " write-to-env {backend} {env}",
                    backend = backend.to_possible_value().unwrap().get_name()
                )?;
            }
            RequestAction::Update {
                file,
                is_secret,
                env_source,
            } => {
                write!(f, " update")?;
                if is_secret {
                    f.write_str(" --is-secret")?;
                }
                if let Some(env_source) = env_source {
                    write!(f, " --env-source {env_source}")?;
                }
                if let Some(file) = file {
                    write!(f, " {}", file.to_str().unwrap())?;
                }
            }
        }

        Ok(())
    }
}

/// (internal) interact with the runtime variable database
#[derive(clap::Args)]
pub struct VarDb {
    /// job idx corresponding to the var db to access
    pub(crate) job_idx: usize,

    /// Runtime variable to access
    var_name: String,

    /// Variable is a raw string, and should be read/written as a plain string.
    #[clap(long)]
    is_raw_string: bool,

    /// Only run if the given variable is true.
    #[clap(long)]
    condvar: Option<String>,

    #[clap(subcommand)]
    action: Option<VarDbAction>,
}

#[derive(clap::Subcommand)]
enum VarDbAction {
    WriteToEnv {
        backend: EnvBackend,
        env: String,
    },
    Update {
        #[clap(long)]
        env_source: Option<String>,
        #[clap(long)]
        is_secret: bool,
        file: Option<PathBuf>,
    },
}

#[derive(clap::ValueEnum, Copy, Clone)]
enum EnvBackend {
    Ado,
    Github,
}

impl VarDb {
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            job_idx,
            var_name,
            is_raw_string,
            condvar,
            action,
        } = self;

        let mut runtime_var_db = open_var_db(job_idx)?;

        if let Some(condvar) = condvar {
            let (condvar_data, _) = runtime_var_db.get_var(&condvar);
            let set: bool = serde_json::from_slice(&condvar_data).unwrap();
            if !set {
                return Ok(());
            }
        }

        let get = |runtime_var_db: &mut Box<dyn RuntimeVarDb>, var_name: &str| {
            let (mut data, data_is_secret) = runtime_var_db.get_var(var_name);
            // HACK: only one kind of db, so we know what routine to use
            if is_raw_string {
                let s: String = serde_json::from_slice(&data).unwrap();
                data = s.into();
            }
            (data, data_is_secret)
        };

        let env_source_name = |env_source| format!(".env.is_secret.{env_source}");

        match action {
            None => {
                // Raw get.
                let (data, _) = get(&mut runtime_var_db, &var_name);
                std::io::stdout().write_all(&data).unwrap();
            }
            Some(VarDbAction::WriteToEnv { backend, env }) => {
                let (data, is_secret) = get(&mut runtime_var_db, &var_name);

                if is_secret {
                    // Remember that this environment variable is secret so that
                    // it cannot be easily laundered into a non-secret variable.
                    runtime_var_db.set_var(&env_source_name(&env), false, "null".into());
                }

                match backend {
                    EnvBackend::Ado => {
                        print!("##vso[task.setvariable variable={env};issecret={is_secret}]");
                        std::io::stdout().write_all(&data).unwrap();
                        println!();
                    }
                    EnvBackend::Github => {
                        let data_string = String::from_utf8(data)?;
                        if is_secret {
                            data_string.lines().for_each(|line| {
                                println!("::add-mask::{}", line);
                            });
                        }
                        let gh_env_file_path = std::env::var("GITHUB_ENV")?;
                        let mut gh_env_file = fs_err::OpenOptions::new()
                            .append(true)
                            .open(gh_env_file_path)?;
                        let gh_env_var_assignment = format!("{}<<EOF\n{}\nEOF\n", env, data_string);
                        gh_env_file.write_all(gh_env_var_assignment.as_bytes())?;
                    }
                }
            }
            Some(VarDbAction::Update {
                env_source,
                mut is_secret,
                file,
            }) => {
                if !is_secret {
                    // If the source environment variable for this was known to
                    // be a secret, then mark it secret.
                    if let Some(env_source) = env_source {
                        is_secret |= runtime_var_db
                            .try_get_var(&env_source_name(&env_source))
                            .is_some();
                    }
                }
                let data = if let Some(file) = file {
                    let mut data = fs_err::read(file)?;
                    // HACK: only one kind of db, so we know what routine to use
                    if is_raw_string {
                        let s: String = String::from_utf8(data).unwrap();
                        data = serde_json::to_vec(&s).unwrap();
                    }
                    data
                } else {
                    let mut data = Vec::new();
                    std::io::stdin().read_to_end(&mut data).unwrap();
                    // HACK: only one kind of db, so we know what routine to use
                    if is_raw_string {
                        // account for bash HEREDOCs including a trailing newline
                        // TODO: probably want this to be configurable.
                        if matches!(data.last(), Some(b'\n')) {
                            data.pop();
                        }

                        let s = String::from_utf8(data).unwrap();
                        data = serde_json::to_vec(&s).unwrap();
                    }
                    data
                };
                runtime_var_db.set_var(&var_name, is_secret, data);
            }
        }

        Ok(())
    }
}

/// Obtain a handle to a runtime var db
///
/// CONTRACT: Requires a pipeline-specific `pipeline.json` file to be in the
/// same dir as the flowey exe
///
/// CONTRACT: Requires a var-backend specific var db file called
/// `job{job_idx}.<ext>` to be in the same dir as the flowey exe
pub(crate) fn open_var_db(job_idx: usize) -> anyhow::Result<Box<dyn RuntimeVarDb>> {
    let current_exe =
        std::env::current_exe().context("failed to get path to current flowey executable")?;

    let FloweyPipelineStaticDb {
        var_db_backend_kind,
        ..
    } = {
        let pipeline_static_db = fs_err::File::open(current_exe.with_file_name("pipeline.json"))?;
        serde_json::from_reader(pipeline_static_db)?
    };

    Ok(match var_db_backend_kind {
        VarDbBackendKind::Json => {
            Box::new(crate::var_db::single_json_file::SingleJsonFileVarDb::new(
                current_exe.with_file_name(format!("job{job_idx}.json")),
            )?)
        }
    })
}
