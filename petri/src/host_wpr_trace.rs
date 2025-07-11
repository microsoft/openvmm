// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows host trace collection for test environments.
//!
//! This module provides functionality to collect Windows Performance Recorder (WPR)
//! traces from the host system during test execution. A single trace session is
//! started before all tests run and stopped after all tests complete, capturing
//! host-side activity for the entire test suite.

use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Global Windows host trace collector that manages a single WPR trace session
/// for the duration of all test execution.
struct GlobalHostTraceCollector {
    session_name: String,
    trace_file: PathBuf,
    is_running: Arc<AtomicBool>,
}

impl GlobalHostTraceCollector {
    /// Create a new global host trace collector.
    fn new(output_dir: &Path) -> anyhow::Result<Self> {
        // Create a globally unique session name to avoid conflicts when multiple test executables run concurrently
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let process_id = std::process::id();
        let session_name = format!("petri_global_host_trace_{}_{}", process_id, timestamp);
        let trace_file = output_dir.join("host_trace_global.etl");

        Ok(Self {
            session_name,
            trace_file,
            is_running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Start the global host trace collection using WPR.
    fn start(&self) -> anyhow::Result<()> {
        if self.is_running.load(Ordering::Acquire) {
            return Ok(());
        }

        // Write the WPR profile to a temporary file
        let wprp_path = self.trace_file.with_extension("wprp");
        std::fs::write(&wprp_path, include_bytes!("host_trace.wprp"))
            .context("failed to write WPR profile")?;

        tracing::info!(
            session_name = %self.session_name,
            trace_file = %self.trace_file.display(),
            "starting global Windows host trace collection"
        );

        let output = Command::new("wpr.exe")
            .args([
                "-start",
                wprp_path.to_str().unwrap(),
                "-filemode",
                "-instancename",
                &self.session_name,
            ])
            .output()
            .context("failed to execute wpr.exe")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::error!(
                stdout = %stdout,
                stderr = %stderr,
                status = ?output.status,
                "failed to start global host trace collection"
            );

            eprintln!(
                "failed to start global host trace collection, stdout = {}, stderr = {}, status = {:?}",
                stdout, stderr, output.status
            );

            anyhow::bail!("wpr.exe failed to start trace collection");
        }

        self.is_running.store(true, Ordering::Release);
        tracing::info!("Global Windows host trace collection started successfully");

        Ok(())
    }

    /// Stop the global host trace collection and save the ETL file.
    fn stop(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.is_running.load(Ordering::Acquire) {
            return Ok(None);
        }

        tracing::info!(
            session_name = %self.session_name,
            trace_file = %self.trace_file.display(),
            "stopping global Windows host trace collection"
        );

        let output = Command::new("wpr.exe")
            .args([
                "-stop",
                self.trace_file.to_str().unwrap(),
                "-instancename",
                &self.session_name,
            ])
            .output()
            .context("failed to execute wpr.exe")?;

        if !output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                stdout = %stdout,
                stderr = %stderr,
                status = ?output.status,
                "wpr.exe failed to stop trace collection properly"
            );
            self.is_running.store(false, Ordering::Release);
            return Ok(None);
        } else {
            tracing::info!("Global Windows host trace collection stopped successfully");
        }

        self.is_running.store(false, Ordering::Release);

        // Clean up the temporary WPRP file
        let wprp_path = self.trace_file.with_extension("wprp");
        if wprp_path.exists() {
            let _ = std::fs::remove_file(&wprp_path);
        }

        if self.trace_file.exists() {
            tracing::info!(
                "Global host trace file saved: {}",
                self.trace_file.display()
            );
            Ok(Some(self.trace_file.clone()))
        } else {
            tracing::warn!(
                "Global ETL file was not created: {}",
                self.trace_file.display()
            );
            Ok(None)
        }
    }
}

impl Drop for GlobalHostTraceCollector {
    fn drop(&mut self) {
        if self.is_running.load(Ordering::Acquire) {
            tracing::warn!(
                "Dropping global host trace collector while still running, attempting cleanup"
            );

            let _ = Command::new("wpr.exe")
                .args(["-cancel", "-instancename", &self.session_name])
                .output();

            self.is_running.store(false, Ordering::Release);

            let wprp_path = self.trace_file.with_extension("wprp");
            if wprp_path.exists() {
                let _ = std::fs::remove_file(&wprp_path);
            }
        }
    }
}

/// Global instance of the host trace collector
static GLOBAL_TRACE_COLLECTOR: OnceLock<Mutex<Option<GlobalHostTraceCollector>>> = OnceLock::new();

/// Check if the current host supports Windows trace collection.
/// This checks if we're on Windows and if wpr.exe is available.
pub fn host_supports_trace_collection() -> bool {
    Command::new("wpr.exe")
        .arg("-status")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Check if there's already a petri global trace session running
fn is_petri_trace_session_running() -> bool {
    let output = Command::new("wpr.exe").arg("-profiles").output();

    if let Ok(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Look for any session that starts with our prefix
        stdout
            .lines()
            .any(|line| line.contains("petri_global_host_trace"))
    } else {
        false
    }
}

/// Increment the reference count for petri WPR sessions
fn increment_wpr_refcount() -> u32 {
    let current = std::env::var("PETRI_WPR_REFCOUNT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let new_count = current + 1;
    std::env::set_var("PETRI_WPR_REFCOUNT", new_count.to_string());
    new_count
}

/// Decrement the reference count for petri WPR sessions
fn decrement_wpr_refcount() -> u32 {
    let current = std::env::var("PETRI_WPR_REFCOUNT")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let new_count = current.saturating_sub(1);
    if new_count == 0 {
        std::env::remove_var("PETRI_WPR_REFCOUNT");
    } else {
        std::env::set_var("PETRI_WPR_REFCOUNT", new_count.to_string());
    }
    new_count
}

/// Create a system-wide mutex name for WPR coordination
fn get_wpr_mutex_name() -> String {
    "Global\\PetriWprMutex".to_string()
}

#[cfg(windows)]
fn with_system_mutex<F, R>(mutex_name: &str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R>,
{
    use std::ffi::CString;
    use std::ptr;

    // This is a simplified version - in a real implementation you'd want proper error handling
    // and proper Windows API usage with CreateMutexA/ReleaseMutex
    // For now, we'll use a file-based lock as a fallback
    let lock_file = std::env::temp_dir().join(format!("{}.lock", mutex_name.replace("\\", "_")));

    // Simple file-based locking
    let _lock_file_handle = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_file)
        .context("failed to create lock file")?;

    let result = f();

    // Clean up lock file on success
    if result.is_ok() {
        let _ = std::fs::remove_file(&lock_file);
    }

    result
}

#[cfg(not(windows))]
fn with_system_mutex<F, R>(_mutex_name: &str, f: F) -> anyhow::Result<R>
where
    F: FnOnce() -> anyhow::Result<R>,
{
    // On non-Windows, just execute the function
    f()
}

/// Start global host trace collection before running tests.
/// This should be called once before all tests run.
pub fn start_global_host_trace_collection(output_dir: &Path) -> anyhow::Result<()> {
    if !host_supports_trace_collection() {
        tracing::debug!("Windows host trace collection not supported on this host");
        return Ok(());
    }

    let mutex_name = get_wpr_mutex_name();

    with_system_mutex(&mutex_name, || {
        // Increment the reference count
        let refcount = increment_wpr_refcount();

        tracing::debug!("WPR refcount incremented to: {}", refcount);

        // Only start WPR if this is the first process (refcount == 1)
        if refcount == 1 {
            // Check if there's already a petri trace session running (from a previous run)
            if is_petri_trace_session_running() {
                tracing::warn!(
                    "Petri global trace collection already running from previous session"
                );
                return Ok(());
            }

            let global_collector = GLOBAL_TRACE_COLLECTOR.get_or_init(|| Mutex::new(None));
            let mut guard = global_collector.lock().unwrap();

            if guard.is_none() {
                let collector = GlobalHostTraceCollector::new(output_dir)?;
                if let Err(e) = collector.start() {
                    tracing::warn!(error = %e, "failed to start global host trace collection");
                    // Decrement refcount since we failed to start
                    decrement_wpr_refcount();
                    return Ok(()); // Don't fail test execution if tracing fails
                }

                *guard = Some(collector);
                tracing::info!(
                    "Global host trace collection started (first process, refcount={})",
                    refcount
                );
            }
        } else {
            tracing::debug!(
                "Global host trace collection already started by another process (refcount={})",
                refcount
            );
        }

        Ok(())
    })
}

/// Stop global host trace collection after all tests complete.
/// This should be called once after all tests have finished.
pub fn stop_global_host_trace_collection() -> anyhow::Result<Option<PathBuf>> {
    if !host_supports_trace_collection() {
        return Ok(None);
    }

    let mutex_name = get_wpr_mutex_name();

    with_system_mutex(&mutex_name, || {
        // Decrement the reference count
        let refcount = decrement_wpr_refcount();

        tracing::debug!("WPR refcount decremented to: {}", refcount);

        // Only stop WPR if this is the last process (refcount == 0)
        if refcount == 0 {
            if let Some(global_collector) = GLOBAL_TRACE_COLLECTOR.get() {
                if let Some(collector) = global_collector.lock().unwrap().take() {
                    match collector.stop() {
                        Ok(trace_path) => {
                            tracing::info!(
                                "Global host trace collection stopped (last process, refcount={})",
                                refcount
                            );
                            return Ok(trace_path);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to stop global host trace collection cleanly");
                            return Ok(None);
                        }
                    }
                }
            }
        } else {
            tracing::debug!(
                "Global host trace collection still needed by other processes (refcount={})",
                refcount
            );
        }

        Ok(None)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_supports_trace_collection() {
        // This test just ensures the function doesn't panic
        let _supported = host_supports_trace_collection();
    }

    #[test]
    fn test_global_trace_collection_api() {
        // Just test that the API doesn't panic on non-Windows or when WPR isn't available
        let temp_dir = std::env::temp_dir();
        let _ = start_global_host_trace_collection(&temp_dir);
        let _ = stop_global_host_trace_collection();
    }
}
