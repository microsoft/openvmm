// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Run shrinkwrap build command to build FVP artifacts.

use flowey::node::prelude::*;
use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread;

flowey_request! {
    pub struct Params {
        pub out_dir: PathBuf,
        pub shrinkwrap_dir: PathBuf,  // Path to shrinkwrap repo (containing shrinkwrap/shrinkwrap executable)
        pub platform_yaml: PathBuf,
        pub overlays: Vec<PathBuf>,
        pub btvars: Vec<String>,      // "KEY=VALUE"
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
            overlays,
            btvars,
            done,
        } = request;

        ctx.emit_rust_step("run shrinkwrap build", |ctx| {
            done.claim(ctx);
            move |_rt| {
                fs_err::create_dir_all(&out_dir)?;
                let log_dir = out_dir.join("logs");
                fs_err::create_dir_all(&log_dir)?;
                let log_path = log_dir.join("shrinkwrap-build.log");

                // Build command line - use shrinkwrap wrapper script with venv activated
                let shrinkwrap_exe = shrinkwrap_dir.join("shrinkwrap").join("shrinkwrap");
                let venv_dir = shrinkwrap_dir.join("venv");
                let venv_bin = venv_dir.join("bin");

                let mut cmd = std::process::Command::new(&shrinkwrap_exe);
                cmd.current_dir(&out_dir); // keep build outputs contained

                // Set environment to use venv Python
                cmd.env("VIRTUAL_ENV", &venv_dir);
                cmd.env("PATH", format!("{}:{}",
                    venv_bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ));

                cmd.arg("build");
                cmd.arg(&platform_yaml);

                for ov in &overlays {
                    cmd.arg("--overlay").arg(ov);
                }

                for bt in &btvars {
                    cmd.arg("--btvar").arg(bt);
                }

                // Stream output to both console and log file
                log::info!("Running shrinkwrap build...");
                log::info!("Output will be saved to: {}", log_path.display());

                cmd.stdout(Stdio::piped());
                cmd.stderr(Stdio::piped());

                let mut child = cmd.spawn()?;

                let stdout = child.stdout.take()
                    .ok_or_else(|| anyhow::anyhow!("failed to capture stdout"))?;
                let stderr = child.stderr.take()
                    .ok_or_else(|| anyhow::anyhow!("failed to capture stderr"))?;

                // Open log file
                let log_file = Arc::new(Mutex::new(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(true)
                        .write(true)
                        .open(&log_path)?
                ));

                // Spawn threads to tee output to both console and log file
                let log_file_clone = log_file.clone();
                let stdout_thread = thread::spawn(move || {
                    let reader = BufReader::new(stdout);
                    for line in reader.lines() {
                        if let Ok(line) = line {
                            println!("{}", line);
                            if let Ok(mut file) = log_file_clone.lock() {
                                let _ = writeln!(file, "{}", line);
                            }
                        }
                    }
                });

                let log_file_clone = log_file.clone();
                let stderr_thread = thread::spawn(move || {
                    let reader = BufReader::new(stderr);
                    for line in reader.lines() {
                        if let Ok(line) = line {
                            eprintln!("{}", line);
                            if let Ok(mut file) = log_file_clone.lock() {
                                let _ = writeln!(file, "STDERR: {}", line);
                            }
                        }
                    }
                });

                // Wait for threads to finish
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();

                // Wait for child process
                let status = child.wait()?;

                if !status.success() {
                    anyhow::bail!(
                        "shrinkwrap build failed (see {})",
                        log_path.display()
                    );
                }

                Ok(())
            }
        });

        Ok(())
    }
}
