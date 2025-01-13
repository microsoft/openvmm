// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::exec_snippet::FloweyPipelineStaticDb;
use super::exec_snippet::VarDbBackendKind;
use anyhow::Context;
use flowey_core::node::RuntimeVarDb;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;

pub fn construct_var_db_cli(
    flowey_bin: &str,
    job_idx: usize,
    var: &str,
    is_secret: bool,
    update_from_stdin: bool,
    update_from_file: Option<&str>,
    is_raw_string: bool,
    write_to_gh_env: Option<String>,
) -> String {
    let mut base = format!(r#"{flowey_bin} v {job_idx} '{var}'"#);

    if update_from_stdin {
        if is_secret {
            base += " --is-secret"
        }

        base += " --update-from-stdin"
    } else if let Some(file) = update_from_file {
        if is_secret {
            base += " --is-secret"
        }

        base = format!("{base} --update-from-file {file}");
    } else if let Some(gh_var) = write_to_gh_env {
        if is_secret {
            base += " --is-secret"
        }

        base = format!("{base} --write-to-gh-env {gh_var}");
    }

    if is_raw_string {
        base += " --is-raw-string"
    }

    base
}

/// (internal) interact with the runtime variable database
#[derive(clap::Args)]
pub struct VarDb {
    /// job idx corresponding to the var db to access
    pub(crate) job_idx: usize,

    /// Runtime variable to access
    var_name: String,

    /// Set the variable by reading from stdin
    #[clap(long, group = "update")]
    update_from_stdin: bool,

    /// Set the variable by reading from a file
    #[clap(long, group = "update")]
    update_from_file: Option<PathBuf>,

    /// Variable is a raw string, and should be read/written as a plain string.
    #[clap(long)]
    is_raw_string: bool,

    /// Whether or not the variable being set if a secret
    #[clap(long, requires = "update")]
    is_secret: bool,

    /// Set the variable as a github environment variable with the given name
    /// rather than printing to stdout.
    #[clap(long, requires = "var_name", group = "update")]
    write_to_gh_env: Option<String>,
}

impl VarDb {
    pub fn run(self) -> anyhow::Result<()> {
        let Self {
            job_idx,
            var_name,
            update_from_stdin,
            update_from_file,
            is_secret,
            is_raw_string,
            write_to_gh_env,
        } = self;

        let mut runtime_var_db = open_var_db(job_idx)?;

        if update_from_stdin {
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

            runtime_var_db.set_var(&var_name, is_secret, data);
        } else if let Some(file) = update_from_file {
            let mut data = fs_err::read(file)?;

            // HACK: only one kind of db, so we know what routine to use
            if is_raw_string {
                let s: String = String::from_utf8(data).unwrap();
                data = serde_json::to_vec(&s).unwrap();
            }

            let var_name = var_name.trim_matches('\'');
            runtime_var_db.set_var(var_name, is_secret, data);
        } else {
            let mut data = runtime_var_db.get_var(&var_name);

            // HACK: only one kind of db, so we know what routine to use
            if is_raw_string {
                let s: String = serde_json::from_slice(&data).unwrap();
                data = s.into();
            }

            if let Some(write_to_gh_env) = write_to_gh_env {
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
                let gh_env_var_assignment = format!(
                    r#"{}<<EOF
{}
EOF
"#,
                    write_to_gh_env, data_string
                );
                gh_env_file.write_all(gh_env_var_assignment.as_bytes())?;
            } else {
                std::io::stdout().write_all(&data).unwrap()
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
