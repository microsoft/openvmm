// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Syscall tracing for child processes.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod ptrace;

#[cfg(target_os = "linux")]
pub use ptrace::TracedChild;
#[cfg(target_os = "linux")]
pub use ptrace::build_seccomp_filter;

/// Syscalls whose behavior is needed to derive filesystem and network policy.
pub const SYSCALLS_TO_TRACE: &[&str] = &[
    // Filesystem access
    "openat",
    "newfstatat",
    "statx",
    "readlinkat",
    "faccessat",
    "faccessat2",
    "mkdirat",
    "unlinkat",
    "renameat2",
    // Descriptor lifecycle shared by filesystem and network access
    "dup",
    "dup2",
    "dup3",
    "fcntl",
    "close",
    // Network access
    "socket",
    "socketpair",
    "bind",
    "connect",
    "listen",
    "accept4",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
];

/// Error returned when a syscall cannot be selected for profile tracing.
#[derive(Debug, thiserror::Error)]
pub enum TraceSyscallError {
    /// A built-in profile syscall has no native syscall number.
    #[error("worker trace parsing support needs to be added for built-in syscall {0:?}")]
    UnsupportedBuiltIn(&'static str),
    /// A configured syscall name is not recognized.
    #[error("worker trace parsing support needs to be added for configured syscall {0:?}")]
    UnsupportedConfigured(String),
}

/// Return whether `name` belongs to the built-in profile tracing set.
pub fn is_profile_trace_syscall(name: &str) -> bool {
    SYSCALLS_TO_TRACE.contains(&name)
}

/// Validate that every configured syscall name is recognized.
pub fn validate_trace_syscalls(
    names: &[String],
    syscall_number: impl Fn(&str) -> Option<i64>,
) -> Result<(), TraceSyscallError> {
    if let Some(name) = names
        .iter()
        .find(|name| {
            syscall_number(name).is_none() && !is_known_without_native_number(name.as_str())
        })
        .cloned()
    {
        return Err(TraceSyscallError::UnsupportedConfigured(name));
    }
    Ok(())
}

/// Resolve profile and configured syscall names to native syscall numbers.
pub fn resolve_trace_syscalls(
    configured: &[String],
    syscall_number: impl Fn(&str) -> Option<i64>,
) -> Result<BTreeMap<i64, String>, TraceSyscallError> {
    validate_trace_syscalls(configured, &syscall_number)?;
    let mut syscalls = BTreeMap::new();
    for &name in SYSCALLS_TO_TRACE {
        match syscall_number(name) {
            Some(number) => {
                syscalls.insert(number, name.to_string());
            }
            None if is_known_without_native_number(name) => continue,
            None => return Err(TraceSyscallError::UnsupportedBuiltIn(name)),
        }
    }
    for name in configured {
        if let Some(number) = syscall_number(name) {
            syscalls.insert(number, name.clone());
        }
    }
    Ok(syscalls)
}

fn is_known_without_native_number(name: &str) -> bool {
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        matches!(name, "create_module" | "dup2" | "fork" | "ioperm" | "iopl")
    }

    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        let _ = name;
        false
    }
}

/// Child-process syscall-tracing configuration.
#[derive(Debug, Clone)]
pub struct TraceConfig {
    /// Directory in which the per-process JSONL trace is written.
    pub output_dir: PathBuf,
    /// Prefix used for trace file names.
    #[cfg(target_os = "linux")]
    file_prefix: String,
    #[cfg(target_os = "linux")]
    syscalls: BTreeMap<i64, String>,
}

impl TraceConfig {
    /// Creates a tracing configuration for the supplied native syscall map.
    #[cfg(target_os = "linux")]
    pub fn new(
        output_dir: PathBuf,
        file_prefix: impl Into<String>,
        syscalls: BTreeMap<i64, String>,
    ) -> Self {
        Self {
            output_dir,
            file_prefix: file_prefix.into(),
            syscalls,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use test_with_tracing::test;

    fn test_syscall_number(name: &str) -> Option<i64> {
        SYSCALLS_TO_TRACE
            .iter()
            .position(|candidate| *candidate == name)
            .map(|index| index as i64)
            .or_else(|| (name == "kill").then_some(1000))
    }

    #[test]
    fn profile_trace_syscalls_include_configured_denials() {
        let syscalls = resolve_trace_syscalls(&["kill".to_string()], test_syscall_number).unwrap();

        assert_eq!(
            syscalls
                .get(&test_syscall_number("openat").unwrap())
                .map(String::as_str),
            Some("openat")
        );
        assert_eq!(syscalls.get(&1000).map(String::as_str), Some("kill"));
        assert!(!is_profile_trace_syscall("kill"));
    }

    #[test]
    fn profile_trace_syscalls_reject_unknown_names() {
        for name in ["not_a_syscall", "umount"] {
            let error =
                resolve_trace_syscalls(&[name.to_string()], test_syscall_number).unwrap_err();
            assert!(matches!(
                error,
                TraceSyscallError::UnsupportedConfigured(rejected) if rejected == name
            ));
        }
    }

    #[test]
    fn profile_trace_syscalls_reject_unresolved_built_ins() {
        let error = resolve_trace_syscalls(&[], |name| {
            if name == "openat" {
                None
            } else {
                test_syscall_number(name)
            }
        })
        .unwrap_err();

        assert!(matches!(
            error,
            TraceSyscallError::UnsupportedBuiltIn("openat")
        ));
    }

    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    #[test]
    fn profile_trace_syscalls_skip_architecture_absent_built_ins() {
        let syscalls = resolve_trace_syscalls(&[], |name| {
            if name == "dup2" {
                None
            } else {
                test_syscall_number(name)
            }
        })
        .unwrap();

        assert_eq!(syscalls.len(), SYSCALLS_TO_TRACE.len() - 1);
        assert!(!syscalls.values().any(|name| name == "dup2"));
        assert_eq!(
            syscalls
                .get(&test_syscall_number("dup3").unwrap())
                .map(String::as_str),
            Some("dup3")
        );
    }
}
