// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Windows host trace collection for test environments.
//!
//! This module provides functionality to collect Windows Performance Recorder (WPR)
//! traces from the host system during test execution. The traces are automatically
//! started at the beginning of a test case and stopped/collected at the end.

use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

use std::process::Command;

use std::sync::Arc;

use std::sync::atomic::{AtomicBool, Ordering};

/// Windows host trace collector that manages WPR trace sessions
/// for the duration of a test case.
pub struct WindowsHostTraceCollector {
    session_name: String,

    trace_file: PathBuf,

    is_running: Arc<AtomicBool>,
}

impl WindowsHostTraceCollector {
    /// Create a new Windows host trace collector for the given test.
    pub fn new(test_name: &str, output_dir: &Path) -> anyhow::Result<Self> {
        {
            let session_name = format!("petri_{}", sanitize_session_name(test_name));
            let trace_file = output_dir.join("host_trace.etl");

            Ok(Self {
                session_name,
                trace_file,
                is_running: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    /// Start the host trace collection using WPR.
    pub fn start(&self) -> anyhow::Result<()> {
        {
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
                "starting Windows host trace collection"
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
                    "failed to start host trace collection"
                );
                anyhow::bail!("wpr.exe failed to start trace collection");
            }

            self.is_running.store(true, Ordering::Release);
            tracing::info!("Windows host trace collection started successfully");
        }

        Ok(())
    }

    /// Stop the host trace collection and save the ETL file.
    /// Returns the path to the saved ETL file, if successful and the trace was actually captured.
    pub fn stop(&self) -> anyhow::Result<Option<PathBuf>> {
        {
            if !self.is_running.load(Ordering::Acquire) {
                return Ok(None);
            }

            tracing::info!(
                session_name = %self.session_name,
                trace_file = %self.trace_file.display(),
                "stopping Windows host trace collection"
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
                // Don't fail the test if we can't stop the trace cleanly
                self.is_running.store(false, Ordering::Release);
                return Ok(None);
            } else {
                tracing::info!("Windows host trace collection stopped successfully");
            }

            self.is_running.store(false, Ordering::Release);

            // Clean up the temporary WPRP file
            let wprp_path = self.trace_file.with_extension("wprp");
            if wprp_path.exists() {
                let _ = std::fs::remove_file(&wprp_path);
            }

            // Register the ETL file as a test attachment by returning its path
            if self.trace_file.exists() {
                tracing::info!("Host trace file saved: {}", self.trace_file.display());
                Ok(Some(self.trace_file.clone()))
            } else {
                tracing::warn!("ETL file was not created: {}", self.trace_file.display());
                Ok(None)
            }
        }
    }

    /// Check if the trace collection is currently running.
    #[cfg(test)]
    fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }

    /// Get the path to the trace file.
    #[cfg(test)]
    fn trace_file(&self) -> &Path {
        &self.trace_file
    }
}

impl Drop for WindowsHostTraceCollector {
    fn drop(&mut self) {
        {
            // Ensure we clean up the trace session on drop
            if self.is_running.load(Ordering::Acquire) {
                tracing::warn!(
                    "Dropping host trace collector while still running, attempting cleanup"
                );

                let _ = Command::new("wpr.exe")
                    .args(["-cancel", "-instancename", &self.session_name])
                    .output();

                self.is_running.store(false, Ordering::Release);

                // Clean up the temporary WPRP file
                let wprp_path = self.trace_file.with_extension("wprp");
                if wprp_path.exists() {
                    let _ = std::fs::remove_file(&wprp_path);
                }
            }
        }
    }
}

/// Sanitize a test name to be used as a WPR session name.
/// WPR session names have restrictions on allowed characters.
fn sanitize_session_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => c,
            _ => '_',
        })
        .collect()
}

/// Check if the current host supports Windows trace collection.
/// This checks if we're on Windows and if wpr.exe is available.

pub fn host_supports_trace_collection() -> bool {
    Command::new("wpr.exe")
        .arg("-status")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Start host trace collection for a test if running on a supported Windows host.
/// Returns `None` if host tracing is not supported or if it fails to start.
pub fn start_host_trace_collection(
    test_name: &str,
    output_dir: &Path,
) -> Option<WindowsHostTraceCollector> {
    {
        if !host_supports_trace_collection() {
            tracing::debug!("Windows host trace collection not supported on this host");
            return None;
        }

        match WindowsHostTraceCollector::new(test_name, output_dir) {
            Ok(collector) => match collector.start() {
                Ok(()) => {
                    tracing::info!("Host trace collection started for test: {}", test_name);
                    Some(collector)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "failed to start host trace collection for test: {}", test_name
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to create host trace collector for test: {}", test_name
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_session_name() {
        assert_eq!(
            sanitize_session_name("test::module::function"),
            "test__module__function"
        );
        assert_eq!(sanitize_session_name("simple_test"), "simple_test");
        assert_eq!(
            sanitize_session_name("test-with-dashes"),
            "test_with_dashes"
        );
        assert_eq!(
            sanitize_session_name("TestWithMixed123"),
            "TestWithMixed123"
        );
        assert_eq!(
            sanitize_session_name("test with spaces"),
            "test_with_spaces"
        );
    }

    #[test]
    fn test_basic_functionality() {
        // Create collectors for different test names and verify they work
        let temp_dir = std::env::temp_dir();

        let collector1 = WindowsHostTraceCollector::new("test_name_1", &temp_dir).unwrap();
        let collector2 = WindowsHostTraceCollector::new("test_name_2", &temp_dir).unwrap();

        // Verify they're not running initially
        assert!(!collector1.is_running());
        assert!(!collector2.is_running());

        // Verify trace file paths are different
        assert_ne!(collector1.trace_file(), collector2.trace_file());
    }

    #[test]
    fn test_host_supports_trace_collection() {
        // This test just ensures the function doesn't panic
        let _supported = host_supports_trace_collection();
    }
}
