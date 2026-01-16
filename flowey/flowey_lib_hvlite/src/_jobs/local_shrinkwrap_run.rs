// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run shrinkwrap run command to launch FVP.

use flowey::node::prelude::*;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

flowey_request! {
    /// Parameters for running Shrinkwrap (FVP launch).
    pub struct Params {
        /// Where to place logs and where to run the command from.
        pub out_dir: PathBuf,
        /// Path to shrinkwrap repo (containing shrinkwrap/shrinkwrap executable)
        pub shrinkwrap_dir: PathBuf,
        /// Path to the platform yaml (e.g. cca-3world.yaml).
        pub platform_yaml: PathBuf,
        /// Rootfs path to pass as --rtvar ROOTFS=<abs path>.
        pub rootfs: PathBuf,
        /// Extra --rtvar KEY=VALUE entries (besides ROOTFS).
        pub rtvars: Vec<String>,
        /// Passthrough args appended to the command line (escape hatch).
        pub extra_args: Vec<String>,
        /// Timeout for the run step. If exceeded, Shrinkwrap process is killed.
        pub timeout_sec: u64,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            out_dir,
            shrinkwrap_dir,
            platform_yaml,
            rootfs,
            rtvars,
            extra_args,
            timeout_sec,
            done,
        } = request;

        ctx.emit_rust_step("run shrinkwrap", |ctx| {
            done.claim(ctx);
            move |_rt| {
                fs::create_dir_all(&out_dir)?;
                let log_dir = out_dir.join("logs");
                fs::create_dir_all(&log_dir)?;
                let console_log_path = log_dir.join("console.log");
                let shrinkwrap_run_log_path = log_dir.join("shrinkwrap-run.log");

                let mut run_log = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&shrinkwrap_run_log_path)?;

                let rootfs_abs = canonicalize_or_abspath(&rootfs)?;
                if !rootfs_abs.exists() {
                    anyhow::bail!("ROOTFS does not exist: {}", rootfs_abs.display());
                }

                // Use shrinkwrap wrapper script with venv activated
                let shrinkwrap_exe = shrinkwrap_dir.join("shrinkwrap").join("shrinkwrap");
                let venv_dir = shrinkwrap_dir.join("venv");
                let venv_bin = venv_dir.join("bin");
                
                let mut cmd = Command::new(&shrinkwrap_exe);
                cmd.current_dir(&out_dir);
                
                // Set environment to use venv Python
                cmd.env("VIRTUAL_ENV", &venv_dir);
                cmd.env("PATH", format!("{}:{}", 
                    venv_bin.display(), 
                    std::env::var("PATH").unwrap_or_default()
                ));
                
                cmd.arg("run");
                cmd.arg(&platform_yaml);
                cmd.arg("--rtvar").arg(format!("ROOTFS={}", rootfs_abs.display()));

                for v in &rtvars {
                    cmd.arg("--rtvar").arg(v);
                }

                for a in &extra_args {
                    cmd.arg(a);
                }

                writeln!(&mut run_log, "cwd: {}", out_dir.display())?;
                writeln!(&mut run_log, "cmd: {}", render_command_for_logs(&cmd))?;
                writeln!(&mut run_log, "timeout_sec: {}", timeout_sec)?;
                run_log.flush()?;

                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());

                let mut child = cmd.spawn().map_err(|e| {
                    anyhow::anyhow!(
                        "failed to spawn shrinkwrap (is it on PATH?): {e}\nlog: {}",
                        shrinkwrap_run_log_path.display()
                    )
                })?;

                let stdout = child.stdout.take()
                    .ok_or_else(|| anyhow::anyhow!("failed to capture shrinkwrap stdout"))?;
                let stderr = child.stderr.take()
                    .ok_or_else(|| anyhow::anyhow!("failed to capture shrinkwrap stderr"))?;

                let console_file = OpenOptions::new()
                    .create(true)
                    .truncate(true)
                    .write(true)
                    .open(&console_log_path)?;
                let console_file = Arc::new(Mutex::new(console_file));

                let t1 = spawn_tee_thread(stdout, console_file.clone(), StreamKind::Stdout);
                let t2 = spawn_tee_thread(stderr, console_file.clone(), StreamKind::Stderr);

                let timeout = Duration::from_secs(timeout_sec);
                let start = Instant::now();

                let exit_status = loop {
                    if let Some(status) = child.try_wait()? {
                        break status;
                    }
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "shrinkwrap run timed out after {}s (killed). See logs:\n- {}\n- {}",
                            timeout_sec,
                            shrinkwrap_run_log_path.display(),
                            console_log_path.display()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(200));
                };

                let _ = t1.join();
                let _ = t2.join();

                if !exit_status.success() {
                    anyhow::bail!(
                        "shrinkwrap run failed (exit={}). See logs:\n- {}\n- {}",
                        exit_status,
                        shrinkwrap_run_log_path.display(),
                        console_log_path.display()
                    );
                }

                Ok(())
            }
        });

        Ok(())
    }
}

#[derive(Copy, Clone)]
enum StreamKind {
    Stdout,
    Stderr,
}

fn spawn_tee_thread<R: io::Read + Send + 'static>(
    reader: R,
    file: Arc<Mutex<File>>,
    kind: StreamKind,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut br = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match br.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if let Ok(mut f) = file.lock() {
                        let _ = f.write_all(line.as_bytes());
                        let _ = f.flush();
                    }
                    match kind {
                        StreamKind::Stdout => {
                            let _ = io::stdout().write_all(line.as_bytes());
                            let _ = io::stdout().flush();
                        }
                        StreamKind::Stderr => {
                            let _ = io::stderr().write_all(line.as_bytes());
                            let _ = io::stderr().flush();
                        }
                    }
                }
                Err(_) => break,
            }
        }
    })
}

fn canonicalize_or_abspath(p: &Path) -> anyhow::Result<PathBuf> {
    if p.exists() {
        Ok(p.canonicalize()?)
    } else if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(p))
    }
}

fn render_command_for_logs(cmd: &Command) -> String {
    let mut s = String::new();
    s.push_str(&cmd.get_program().to_string_lossy());
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&shell_escape(a.to_string_lossy().as_ref()));
    }
    s
}

fn shell_escape(arg: &str) -> String {
    if arg.is_empty() {
        "''".to_string()
    } else if arg.bytes().all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b'=' | b':' | b'+' )) {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}
