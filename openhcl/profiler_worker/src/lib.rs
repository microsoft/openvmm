// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A worker for profiling on VTL2.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]
#![forbid(unsafe_code)]

use anyhow::Context;
use futures::FutureExt;
use mesh::error::RemoteError;
use mesh::MeshPayload;
use mesh_worker::Worker;
use mesh_worker::WorkerId;
use mesh_worker::WorkerRpc;
use pal_async::driver::Driver;
use pal_async::timer::PolledTimer;
use pal_async::DefaultPool;
use socket2::Socket;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::pin::pin;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

/// Minimum memory for profiler to start (10MB)
/// This is to make sure VTL2 doesn't OOM before the Profiler Memory check can take effect
const MIN_MEMORY_PROFILER_MB: u64 = 10;

/// Struct for profiler worker to store the request value
#[derive(Debug, MeshPayload)]
pub struct ProfilerRequest {
    /// Profiling duration in seconds
    pub duration: u64,
    /// List of profiler arguments to pass in the bin file
    pub profiler_args: Vec<String>,
    /// Socket connection where bin file will be written to
    pub conn: Socket,
}

/// The worker ID.
pub const PROFILER_WORKER: WorkerId<ProfilerWorkerParameters> = WorkerId::new("ProfilerWorker");

/// The profiler worker parameter
#[derive(MeshPayload)]
pub struct ProfilerWorkerParameters {
    /// Profiler Request struct
    pub profiler_request: ProfilerRequest,
}

/// The profiler worker struct
pub struct ProfilerWorker {
    profiler_request: ProfilerRequest,
}

impl Worker for ProfilerWorker {
    type Parameters = ProfilerWorkerParameters;
    type State = ();
    const ID: WorkerId<Self::Parameters> = WorkerId::new("ProfilerWorker");

    /// Create new worker and store the request
    fn new(parameters: Self::Parameters) -> anyhow::Result<Self> {
        Ok(Self {
            profiler_request: parameters.profiler_request,
        })
    }

    /// Profiler worker is run per Profile request so there is no need for restart
    fn restart(_state: Self::State) -> anyhow::Result<Self> {
        unimplemented!()
    }

    /// Run profiler worker and start a profiling session
    fn run(self, mut recv: mesh::Receiver<WorkerRpc<Self::State>>) -> anyhow::Result<()> {
        DefaultPool::run_with(|driver| async move {
            let mut profiling = pin!(profile(self.profiler_request, &driver).fuse());
            loop {
                let msg = futures::select! { // merge semantics
                    msg = recv.recv().fuse() => {
                        msg
                    },
                    r = profiling => {
                        match r {
                            Ok(_) => {
                                break
                            },
                            Err(err) => {
                                anyhow::bail!("Profiling failed - Error {}", err.to_string());
                            }
                        }
                    }
                };
                match msg {
                    Ok(message) => match message {
                        WorkerRpc::Stop => {
                            break;
                        }
                        WorkerRpc::Restart(response) => {
                            response.send(Err(RemoteError::new(anyhow::anyhow!("not supported"))));
                        }
                        WorkerRpc::Inspect(_deferred) => {}
                    },
                    Err(err) => {
                        anyhow::bail!("ProfilerWorker::Run - Error {}", err.to_string());
                    }
                }
            }
            Ok(())
        })
    }
}

/// Get current free memory in MB
fn get_free_mem_mb() -> anyhow::Result<u64> {
    parse_meminfo_free(&fs_err::read_to_string("/proc/meminfo")?)
}

fn parse_meminfo_free(contents: &str) -> anyhow::Result<u64> {
    const KBYTES_PER_MBYTES: u64 = 1024;
    for line in contents.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name == "MemFree" {
            let value = rest
                .split_ascii_whitespace()
                .next()
                .context("line had no value")?;
            let value = value.parse::<u64>().context("value failed to parse")?;
            return Ok(value / KBYTES_PER_MBYTES);
        }
    }

    Err(anyhow::anyhow!("no memfree line found"))
}

/// Profiling function for the worker
pub async fn profile(request: ProfilerRequest, driver: &impl Driver) -> anyhow::Result<()> {
    let mut timer = PolledTimer::new(driver);
    let ProfilerRequest {
        conn,
        duration,
        mut profiler_args,
    } = request;

    // Set CLOEXEC to false because we need to share FD with child process
    conn.set_cloexec(false)
        .map_err(anyhow::Error::from)
        .context("Failed to set CLO_EXEC to Socket")?;

    let socket_fd = conn.as_raw_fd();
    let free_mem_mb = match get_free_mem_mb() {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("Error when getting memory {}", e.to_string());
            0
        }
    };

    if free_mem_mb < MIN_MEMORY_PROFILER_MB {
        anyhow::bail!("Not enough memory to start profiler {} MB", free_mem_mb);
    }

    // Limit memory to 75% of free memory
    profiler_args.push(format!("LimitMB:{}", free_mem_mb * 75 / 100));

    let mut process = Command::new("/usr/bin/underhill_profiler_binary")
        .arg(duration.to_string())
        .arg(socket_fd.to_string())
        .args(profiler_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to execute process")?;

    let mut process_success = false;

    // Sleep for duration+1s so the child process can finish naturally
    timer.sleep(Duration::from_secs(duration + 1)).await;

    // Checking if child process finished every 1s for 15s
    // This is a failsafe in case child process doesn't exit and run
    // forever (which shouldn't happen unless something went wrong)
    for wait_time in 1..=15 {
        match process.try_wait() {
            Ok(Some(_status)) => {
                process_success = true;
                break;
            }
            Ok(None) => {
                if wait_time == 15 {
                    tracing::error!("Running profiler binary timeout");
                    if let Err(e) = process.kill() {
                        tracing::error!(
                            e = &e as &dyn std::error::Error,
                            "Error when stopping child process"
                        );
                    }
                    process_success = false;
                }
            }
            Err(e) => {
                process_success = false;
                tracing::error!(
                    "Running profiler binary failed with error {}",
                    e.to_string()
                );
                break;
            }
        }
        timer.sleep(Duration::from_secs(1)).await;
    }

    // Get Stdout and Stderr content
    let mut child_stdout = process.stdout.take().unwrap();
    let mut child_stderr = process.stderr.take().unwrap();

    let mut buffer = Vec::new();

    let _ = child_stdout.read_to_end(&mut buffer);

    if !buffer.is_empty() {
        tracing::info!("{}", String::from_utf8(buffer).unwrap());
    }

    let mut buffer = Vec::new();

    let _ = child_stderr.read_to_end(&mut buffer);

    if !buffer.is_empty() {
        tracing::error!("{}", String::from_utf8(buffer).unwrap());
    }

    // Drop socket no matter child process succeeded or not
    drop(conn);
    if !process_success {
        anyhow::bail!("Failed while running `underhill_profiler_binary`")
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_meminfo_free() {
        assert_eq!(
            super::parse_meminfo_free("MemFree: 1048575 kB").unwrap(),
            1023
        );
        assert_eq!(
            super::parse_meminfo_free("MemFree:1048576 kB").unwrap(),
            1024
        );
        assert_eq!(
            super::parse_meminfo_free("MemFree:  1048577 kB").unwrap(),
            1024
        );
    }

    #[test]
    fn test_parse_meminfo_free_real_data() {
        let contents = "MemTotal:       32658576 kB\nMemFree:        11105884 kB\nMemAvailable:   26792856 kB\nBuffers:          828448 kB\nCached:         14789464 kB";
        assert_eq!(super::parse_meminfo_free(contents).unwrap(), 10845);
    }
}
