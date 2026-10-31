// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build candidate OpenVMM sandbox profiles from worker syscall traces.

#![forbid(unsafe_code)]

use anyhow::Context;
use anyhow::ensure;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::BufRead;
use std::net::IpAddr;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

const AT_FDCWD: u64 = u32::MAX as u64 - 99;
const O_ACCMODE: u64 = 0o3;
const O_WRONLY: u64 = 0o1;
const O_RDWR: u64 = 0o2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_TMPFILE: u64 = 0o20_200_000;
const AF_UNIX: u64 = 1;
const AF_INET: u64 = 2;
const AF_INET6: u64 = 10;

/// Inputs used to generate one worker profile.
#[derive(Debug)]
pub struct TraceOptions {
    /// Directory containing worker trace JSONL files, recursively searched.
    pub trace_dir: PathBuf,
    /// Worker name used to select trace files and name the generated profile.
    pub worker: String,
    /// Directory where the generated Rust source file is written.
    pub output_dir: PathBuf,
}

/// Summary of a generated profile.
#[derive(Debug)]
pub struct TraceReport {
    /// Generated Rust source file.
    pub profile_path: PathBuf,
    /// Trace files consumed.
    pub trace_files: Vec<PathBuf>,

    /// Filesystem accesses observed in the trace.
    pub observed_filesystem: Vec<String>,
    /// Network operations observed in the trace.
    pub observed_network: Vec<String>,
    /// Syscall names observed in the trace.
    pub observed_syscalls: Vec<String>,

    /// Read-only directory grants emitted, with execute access outside sysfs.
    pub policy_read_paths: Vec<PathBuf>,
    /// Read-write directory grants emitted.
    pub policy_read_write_paths: Vec<PathBuf>,
    /// Filesystem behavior that was not converted into an explicit grant.
    pub policy_filesystem_access_omissions: Vec<String>,
    /// Network policy generated from the trace.
    pub policy_network: sandbox::Network,
    /// Default syscall denials that were not observed and were emitted.
    pub policy_syscalls: Vec<String>,
}

// Stores trace report entries, fd-to-resource mappings needed by later events, and
// the policy derived from those events as separate state.
#[derive(Debug, Default)]
struct TraceAnalysis {
    observed_filesystem: BTreeSet<String>,
    observed_network: BTreeSet<String>,
    observed_syscalls: BTreeSet<String>,

    filesystem_fd_paths: BTreeMap<i64, PathBuf>,
    network_fd_scopes: BTreeMap<i64, sandbox::Network>,

    policy_read_paths: BTreeSet<PathBuf>,
    policy_read_write_paths: BTreeSet<PathBuf>,
    policy_filesystem_access_omissions: BTreeSet<String>,
    policy_network: sandbox::Network,
}

/// Generate a sandbox profile template from the worker trace.
pub fn build_profile(trace_options: &TraceOptions) -> anyhow::Result<TraceReport> {
    ensure!(
        trace_options.trace_dir.is_dir(),
        "trace directory does not exist: {}",
        trace_options.trace_dir.display()
    );

    let worker_file_name = sanitize_file_component(&trace_options.worker);
    ensure!(!worker_file_name.is_empty(), "worker name is empty");
    let rust_identifier = sanitize_rust_identifier(&trace_options.worker);
    let module_name = format!("{rust_identifier}_worker");
    let syscall_denylist = sandbox::platform_syscall_denylist()
        .copied()
        .collect::<Vec<_>>();
    let default_syscall_denials = syscall_denylist
        .iter()
        .map(|entry| entry.name.to_string())
        .collect::<Vec<_>>();
    #[cfg(target_os = "linux")]
    pal_tracer::validate_trace_syscalls(&default_syscall_denials, sandbox::nr_for_name)?;
    let trace_files = find_worker_traces(&trace_options.trace_dir, &worker_file_name)?;
    ensure!(
        !trace_files.is_empty(),
        "no JSONL traces for worker '{}' found under {}",
        trace_options.worker,
        trace_options.trace_dir.display()
    );

    let mut trace_analysis = TraceAnalysis::default();
    for path in &trace_files {
        ingest_trace(path, &default_syscall_denials, &mut trace_analysis)?;
    }
    normalize_grants(&mut trace_analysis);

    fs_err::create_dir_all(&trace_options.output_dir)
        .with_context(|| format!("failed to create {}", trace_options.output_dir.display()))?;
    let profile_path = trace_options.output_dir.join(format!("{module_name}.rs"));
    let source = render_profile(
        &trace_options.worker,
        &module_name,
        &trace_files,
        &trace_analysis,
        &syscall_denylist,
    );
    fs_err::write(&profile_path, source)
        .with_context(|| format!("failed to write {}", profile_path.display()))?;
    let policy_syscalls = generated_syscall_denials(&syscall_denylist, &trace_analysis);
    let policy_network = trace_analysis.policy_network;

    Ok(TraceReport {
        profile_path,
        trace_files,
        observed_filesystem: trace_analysis.observed_filesystem.into_iter().collect(),
        observed_network: trace_analysis.observed_network.into_iter().collect(),
        observed_syscalls: trace_analysis.observed_syscalls.into_iter().collect(),
        policy_read_paths: trace_analysis.policy_read_paths.into_iter().collect(),
        policy_read_write_paths: trace_analysis.policy_read_write_paths.into_iter().collect(),
        policy_filesystem_access_omissions: trace_analysis
            .policy_filesystem_access_omissions
            .into_iter()
            .collect(),
        policy_network,
        policy_syscalls,
    })
}

fn find_worker_traces(root: &Path, worker: &str) -> anyhow::Result<Vec<PathBuf>> {
    let mut pending = vec![root.to_path_buf()];
    let mut traces = Vec::new();
    let current_prefix = format!("worker-{worker}.");
    let legacy_prefix = format!("{worker}.");

    while let Some(dir) = pending.pop() {
        for entry in fs_err::read_dir(&dir)
            .with_context(|| format!("failed to read trace directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
                && (name.starts_with(&current_prefix) || name.starts_with(&legacy_prefix))
            {
                traces.push(path);
            }
        }
    }
    traces.sort();
    Ok(traces)
}

// Process events in recorded order because dup and close change what an fd
// refers to when that fd appears in later events.
fn ingest_trace(
    path: &Path,
    configured_denials: &[String],
    trace_analysis: &mut TraceAnalysis,
) -> anyhow::Result<()> {
    // Each trace is a separate worker run. An fd number in the next trace does
    // not refer to the resource tracked under that number in this trace.
    trace_analysis.filesystem_fd_paths.clear();
    trace_analysis.network_fd_scopes.clear();
    let file = fs_err::File::open(path)
        .with_context(|| format!("failed to open trace {}", path.display()))?;
    for (index, line) in std::io::BufReader::new(file).lines().enumerate() {
        let line =
            line.with_context(|| format!("failed to read {}:{}", path.display(), index + 1))?;
        let event: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at {}:{}", path.display(), index + 1))?;
        if event["event"] != "syscall" {
            continue;
        }

        let Some(syscall) = event["syscall"].as_str().filter(|syscall| {
            pal_tracer::is_profile_trace_syscall(syscall)
                || configured_denials.iter().any(|name| name == syscall)
        }) else {
            continue;
        };
        trace_analysis.observed_syscalls.insert(syscall.to_string());

        let decoded = event.get("decoded_args").or_else(|| event.get("details"));
        let args = parse_args(&event);
        let result = event["result"].as_i64();
        let errno = event["errno"].as_i64();
        observe_fd_lifecycle(syscall, args, result, trace_analysis);
        observe_network(syscall, args, result, errno, decoded, trace_analysis);

        if let Some(path) = decoded
            .and_then(|decoded| decoded.get("path"))
            .and_then(Value::as_str)
        {
            observe_path(syscall, args, path, result, trace_analysis);
        }
        if let Some(path) = decoded
            .and_then(|decoded| decoded.get("path2"))
            .and_then(Value::as_str)
        {
            let mut path_args = args;
            path_args[0] = args[2];
            observe_path(syscall, path_args, path, result, trace_analysis);
        }
    }
    Ok(())
}

fn parse_args(event: &Value) -> [u64; 6] {
    let mut args = [0; 6];
    if let Some(values) = event["args"].as_array() {
        for (destination, value) in args.iter_mut().zip(values) {
            *destination = value.as_u64().unwrap_or_default();
        }
    }
    args
}

fn observe_path(
    syscall: &str,
    args: [u64; 6],
    raw_path: &str,
    result: Option<i64>,
    trace_analysis: &mut TraceAnalysis,
) {
    let is_write = path_is_written(syscall, args);
    let access = if is_write { "RW" } else { "RO" };
    let Some(path) = resolve_filesystem_path(raw_path, args[0], trace_analysis) else {
        trace_analysis
            .observed_filesystem
            .insert(format!("{raw_path} ({access})"));
        trace_analysis
            .policy_filesystem_access_omissions
            .insert(format!(
                "Relative path {raw_path} from {syscall} was not converted to a grant (dirfd: {}).",
                format_dirfd(args[0])
            ));
        return;
    };

    trace_analysis
        .observed_filesystem
        .insert(format!("{} ({access})", path.display()));

    if syscall == "openat"
        && let Some(fd) = result.filter(|result| *result >= 0)
    {
        // Remember the returned path so a later relative *at call using this
        // fd can be resolved to an absolute path.
        trace_analysis.filesystem_fd_paths.insert(fd, path.clone());
    }

    // The sandbox creates its own /proc and /tmp mounts. Granting the host
    // versions would give the worker different access from what it requested.
    if path.starts_with("/proc") {
        trace_analysis
            .policy_filesystem_access_omissions
            .insert(format!(
                "{} is provided by the sandbox's implicit /proc mount.",
                path.display()
            ));
        return;
    }
    if path.starts_with("/tmp") {
        trace_analysis
            .policy_filesystem_access_omissions
            .insert(format!(
            "{} is under the sandbox's implicit empty writable /tmp; verify that host contents are not required.",
            path.display()
        ));
        return;
    }

    let grant_path = directory_grant(&path);
    // Granting / would expose the entire host filesystem. Report the access for
    // review instead of generating that grant.
    if grant_path == Path::new("/") {
        trace_analysis
            .policy_filesystem_access_omissions
            .insert(format!(
                "{} would require granting /; no filesystem grant was generated.",
                path.display()
            ));
        return;
    }

    if is_write {
        trace_analysis.policy_read_write_paths.insert(grant_path);
    } else {
        trace_analysis.policy_read_paths.insert(grant_path);
    }
}

fn resolve_filesystem_path(
    raw_path: &str,
    dirfd: u64,
    trace_analysis: &TraceAnalysis,
) -> Option<PathBuf> {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        return Some(normalize_path(path));
    }
    if dirfd == AT_FDCWD {
        // An AT_FDCWD path depends on the worker's current directory. The trace
        // does not record that directory, so do not guess an absolute path.
        return None;
    }
    trace_analysis
        .filesystem_fd_paths
        .get(&(dirfd as i64))
        .map(|base| normalize_path(&base.join(path)))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

fn directory_grant(path: &Path) -> PathBuf {
    // The sandbox grants directory trees, so an accessed file becomes a grant
    // for its parent directory.
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(Path::new("/")).to_path_buf()
    }
}

fn path_is_written(syscall: &str, args: [u64; 6]) -> bool {
    match syscall {
        "openat" => {
            let flags = args[2];
            matches!(flags & O_ACCMODE, O_WRONLY | O_RDWR)
                || flags & (O_CREAT | O_TRUNC | O_APPEND) != 0
                || flags & O_TMPFILE == O_TMPFILE
        }
        "mkdirat" | "unlinkat" | "renameat2" => true,
        _ => false,
    }
}

fn observe_network(
    syscall: &str,
    args: [u64; 6],
    result: Option<i64>,
    errno: Option<i64>,
    decoded: Option<&Value>,
    trace_analysis: &mut TraceAnalysis,
) {
    // socket identifies the address family. Later calls such as bind and
    // connect determine whether the worker needs loopback or broader access.
    match syscall {
        "socket" => {
            let domain = args[0];
            if let Some(fd) = result.filter(|result| *result >= 0)
                && domain != AF_UNIX
            {
                trace_analysis
                    .observed_network
                    .insert(format!("{} (address family)", address_family_name(domain)));
                if matches!(domain, AF_INET | AF_INET6) {
                    trace_analysis
                        .network_fd_scopes
                        .insert(fd, sandbox::Network::None);
                }
            }
        }
        "accept4" => {
            if let (Some(listener_scope), Some(accepted_fd)) = (
                trace_analysis
                    .network_fd_scopes
                    .get(&(args[0] as i64))
                    .copied(),
                result.filter(|result| *result >= 0),
            ) {
                // If the trace does not show whether the listener is bound
                // only to loopback, assume it may accept non-loopback
                // connections and use Unrestricted.
                let scope = if listener_scope == sandbox::Network::None {
                    sandbox::Network::Unrestricted
                } else {
                    listener_scope
                };
                trace_analysis
                    .observed_network
                    .insert(format!("fd {} (network descriptor)", args[0]));
                trace_analysis.network_fd_scopes.insert(accepted_fd, scope);
                trace_analysis.policy_network = trace_analysis.policy_network.max(scope);
            }
        }
        "connect" | "bind" | "sendto" if syscall_succeeded(syscall, result, errno) => {
            let fd = args[0] as i64;
            if let Some((scope, endpoint)) = socket_address_scope(decoded) {
                trace_analysis.network_fd_scopes.entry(fd).or_default();
                trace_analysis
                    .observed_network
                    .insert(format!("{endpoint} (endpoint)"));
                widen_network_fd(fd, scope, trace_analysis);
            } else if let Some(current_scope) = trace_analysis.network_fd_scopes.get(&fd).copied() {
                let (scope, endpoint) = {
                    // sendto without an address uses the socket's connected
                    // peer, so keep its known scope. For other missing address
                    // data, use Unrestricted because loopback is not proven.
                    let scope = if syscall == "sendto" && current_scope != sandbox::Network::None {
                        current_scope
                    } else {
                        sandbox::Network::Unrestricted
                    };
                    (scope, "unknown address".to_string())
                };
                trace_analysis
                    .observed_network
                    .insert(format!("{endpoint} (endpoint)"));
                widen_network_fd(fd, scope, trace_analysis);
            }
        }
        "listen" | "recvfrom" | "sendmsg" | "recvmsg"
            if result.is_some_and(|result| result >= 0) =>
        {
            let fd = args[0] as i64;
            if let Some(scope) = trace_analysis.network_fd_scopes.get(&fd).copied() {
                // Successful I/O proves that networking is required. If no
                // endpoint was decoded, use Unrestricted because the trace
                // does not show that the traffic stayed on loopback.
                let scope = if scope == sandbox::Network::None {
                    sandbox::Network::Unrestricted
                } else {
                    scope
                };
                trace_analysis
                    .observed_network
                    .insert(format!("fd {fd} (network descriptor)"));
                widen_network_fd(fd, scope, trace_analysis);
            }
        }
        _ => {}
    }
}

fn observe_fd_lifecycle(
    syscall: &str,
    args: [u64; 6],
    result: Option<i64>,
    trace_analysis: &mut TraceAnalysis,
) {
    // dup creates another fd for the same resource, so copy its tracked path or
    // network scope. close removes the mapping before the fd number is reused.
    match syscall {
        "dup" => copy_tracked_fd(args[0], result, trace_analysis),
        "dup2" => copy_tracked_fd(
            args[0],
            result.filter(|fd| *fd >= 0).map(|_| args[1] as i64),
            trace_analysis,
        ),
        "dup3" => copy_tracked_fd(
            args[0],
            result.filter(|fd| *fd >= 0).map(|_| args[1] as i64),
            trace_analysis,
        ),
        "fcntl" if matches!(args[1], 0 | 1030) => {
            copy_tracked_fd(args[0], result, trace_analysis);
        }
        "close" if result == Some(0) => {
            let fd = args[0] as i64;
            trace_analysis.filesystem_fd_paths.remove(&fd);
            trace_analysis.network_fd_scopes.remove(&fd);
        }
        _ => {}
    }
}

fn copy_tracked_fd(source: u64, destination: Option<i64>, trace_analysis: &mut TraceAnalysis) {
    let Some(destination) = destination.filter(|fd| *fd >= 0) else {
        return;
    };
    let source = source as i64;
    if let Some(path) = trace_analysis.filesystem_fd_paths.get(&source).cloned() {
        trace_analysis.filesystem_fd_paths.insert(destination, path);
    }
    if let Some(scope) = trace_analysis.network_fd_scopes.get(&source).copied() {
        trace_analysis.network_fd_scopes.insert(destination, scope);
    }
}

fn syscall_succeeded(syscall: &str, result: Option<i64>, errno: Option<i64>) -> bool {
    if result.is_some_and(|result| result >= 0) {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        // EINPROGRESS means a nonblocking connection was started, so count it
        // as network use even though connect has not completed yet.
        syscall == "connect" && errno == Some(libc::EINPROGRESS as i64)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (syscall, errno);
        false
    }
}

fn socket_address_scope(decoded: Option<&Value>) -> Option<(sandbox::Network, String)> {
    let socket_address = decoded?.get("socket_address")?;
    let address = socket_address.get("address")?.as_str()?;
    let port = socket_address.get("port")?.as_u64()?;
    let address = address.parse::<IpAddr>().ok()?;
    let scope = if address.is_loopback() {
        sandbox::Network::Loopback
    } else {
        sandbox::Network::Unrestricted
    };
    Some((scope, format!("{address}:{port}")))
}

fn widen_network_fd(fd: i64, scope: sandbox::Network, trace_analysis: &mut TraceAnalysis) {
    // Keep the broadest requirement seen for this fd and for the worker. A
    // later loopback event must not undo an earlier unrestricted event.
    let current = trace_analysis
        .network_fd_scopes
        .get(&fd)
        .copied()
        .unwrap_or_default();
    trace_analysis
        .network_fd_scopes
        .insert(fd, current.max(scope));
    trace_analysis.policy_network = trace_analysis.policy_network.max(scope);
}

fn normalize_grants(trace_analysis: &mut TraceAnalysis) {
    // Remove grants already covered by a broader grant. Read-write covers
    // read-only, and a parent directory covers its child directories.
    trace_analysis
        .policy_read_paths
        .retain(|path| !is_covered(path, &trace_analysis.policy_read_write_paths));
    trace_analysis.policy_read_paths = collapse_paths(&trace_analysis.policy_read_paths);
    trace_analysis.policy_read_write_paths =
        collapse_paths(&trace_analysis.policy_read_write_paths);
}

fn collapse_paths(paths: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut collapsed = BTreeSet::new();
    for path in paths {
        if !is_covered(path, &collapsed) {
            collapsed.insert(path.clone());
        }
    }
    collapsed
}

fn is_covered(path: &Path, grants: &BTreeSet<PathBuf>) -> bool {
    grants.iter().any(|grant| path.starts_with(grant))
}

fn render_profile(
    worker: &str,
    function_name: &str,
    trace_files: &[PathBuf],
    trace_analysis: &TraceAnalysis,
    syscall_denylist: &[sandbox::DeniedSyscall],
) -> String {
    // Include the observed accesses and omitted grants beside the generated
    // policy so a reviewer can compare the policy with its trace evidence.
    let generated_denials = generated_syscall_denials(syscall_denylist, trace_analysis);
    let generated_network = trace_analysis.policy_network;
    let mut source = String::new();
    writeln!(source, "// Copyright (c) Microsoft Corporation.").unwrap();
    writeln!(source, "// Licensed under the MIT License.\n").unwrap();
    writeln!(
        source,
        "//! Automatically generated sandbox profile for {worker}."
    )
    .unwrap();
    writeln!(source, "//!").unwrap();
    writeln!(
        source,
        "//! Generated by sandbox_profiler from {} trace file(s).",
        trace_files.len()
    )
    .unwrap();
    write_comment_list(
        &mut source,
        "Observed filesystem accesses",
        trace_analysis.observed_filesystem.iter().cloned(),
    );
    write_comment_list(
        &mut source,
        "Observed network accesses",
        trace_analysis.observed_network.iter().cloned(),
    );
    write_comment_list(
        &mut source,
        "Observed syscalls",
        trace_analysis.observed_syscalls.iter().cloned(),
    );
    write_comment_list(
        &mut source,
        "Filesystem policy access omissions",
        trace_analysis
            .policy_filesystem_access_omissions
            .iter()
            .cloned(),
    );
    write_comment_list(
        &mut source,
        "Syscalls denied by policy but used by the worker",
        observed_policy_denials(syscall_denylist, trace_analysis),
    );
    writeln!(source).unwrap();
    writeln!(source, "use crate::Builder;").unwrap();
    writeln!(source, "use crate::Network;").unwrap();
    writeln!(source, "use crate::Profile;").unwrap();
    writeln!(source, "use crate::Syscalls;\n").unwrap();
    writeln!(
        source,
        "/// Automatically generated profile from observed worker behavior."
    )
    .unwrap();
    writeln!(source, "pub fn {function_name}() -> Builder {{").unwrap();
    writeln!(source, "    Profile::deny_all()").unwrap();
    writeln!(source, "        .name({})", rust_string(function_name)).unwrap();
    for path in &trace_analysis.policy_read_paths {
        // Traces do not capture executable mappings. Keep execution available
        // outside sysfs so that shared libraries remain loadable.
        let method = if path.starts_with("/sys") {
            "read"
        } else {
            "read_exec"
        };
        writeln!(
            source,
            "        .{method}({})",
            rust_string(&path.display().to_string())
        )
        .unwrap();
    }
    for path in &trace_analysis.policy_read_write_paths {
        writeln!(
            source,
            "        .read_write({})",
            rust_string(&path.display().to_string())
        )
        .unwrap();
    }
    writeln!(source, "        .syscalls(Syscalls::deny([").unwrap();
    for syscall in &generated_denials {
        writeln!(source, "            {},", rust_string(syscall)).unwrap();
    }
    writeln!(source, "        ]))").unwrap();
    writeln!(
        source,
        "        .network(Network::{})",
        network_variant(generated_network)
    )
    .unwrap();
    writeln!(source, "}}").unwrap();
    source
}

fn write_comment_list(
    output: &mut String,
    heading: &str,
    values: impl IntoIterator<Item = String>,
) {
    writeln!(output, "//!").unwrap();
    writeln!(output, "//! {heading}:").unwrap();
    let mut any = false;
    for value in values {
        any = true;
        writeln!(output, "//! - {value}").unwrap();
    }
    if !any {
        writeln!(output, "//! - None.").unwrap();
    }
}

fn generated_syscall_denials(
    syscall_denylist: &[sandbox::DeniedSyscall],
    trace_analysis: &TraceAnalysis,
) -> Vec<String> {
    // Mandatory denials remain enforced. Optional denials are omitted when the
    // trace proves that the worker requires the syscall.
    syscall_denylist
        .iter()
        .filter(|entry| {
            entry.enforcement == sandbox::Enforcement::Mandatory
                || !syscall_was_observed(entry.name, trace_analysis)
        })
        .map(|entry| entry.name.to_string())
        .collect()
}

fn observed_policy_denials(
    syscall_denylist: &[sandbox::DeniedSyscall],
    trace_analysis: &TraceAnalysis,
) -> Vec<String> {
    // List denylisted syscalls used by the worker so reviewers can decide
    // whether to change the worker or the denylist.
    syscall_denylist
        .iter()
        .filter(|entry| syscall_was_observed(entry.name, trace_analysis))
        .map(|entry| entry.name.to_string())
        .collect()
}

fn syscall_was_observed(name: &str, trace_analysis: &TraceAnalysis) -> bool {
    trace_analysis.observed_syscalls.contains(name)
}

fn sanitize_file_component(worker: &str) -> String {
    worker
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn sanitize_rust_identifier(worker: &str) -> String {
    sanitize_file_component(worker).replace('-', "_")
}

fn format_dirfd(dirfd: u64) -> String {
    if dirfd == AT_FDCWD {
        "AT_FDCWD".to_string()
    } else {
        dirfd.to_string()
    }
}

fn address_family_name(domain: u64) -> String {
    match domain {
        0 => "AF_UNSPEC".to_string(),
        1 => "AF_UNIX".to_string(),
        2 => "AF_INET".to_string(),
        10 => "AF_INET6".to_string(),
        16 => "AF_NETLINK".to_string(),
        17 => "AF_PACKET".to_string(),
        _ => domain.to_string(),
    }
}

fn network_variant(network: sandbox::Network) -> &'static str {
    match network {
        sandbox::Network::None => "None",
        sandbox::Network::Loopback => "Loopback",
        sandbox::Network::Unrestricted => "Unrestricted",
    }
}

fn rust_string(value: &str) -> String {
    format!("{value:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_profile_from_current_trace_schema() {
        let temp = tempfile::tempdir().unwrap();
        let traces = temp.path().join("traces");
        fs_err::create_dir_all(&traces).unwrap();
        fs_err::write(
            traces.join("worker-vm.42.jsonl"),
            concat!(
                "{\"event\":\"syscall\",\"syscall\":\"openat\",\"sys_nr\":257,",
                "\"args\":[4294967196,1,524288,0,0,0],",
                "\"decoded_args\":{\"path\":\"/usr/lib/libexample.so\"},\"result\":3}\n",
                "{\"event\":\"syscall\",\"syscall\":\"openat\",\"sys_nr\":257,",
                "\"args\":[4294967196,1,524288,0,0,0],",
                "\"decoded_args\":{\"path\":\"/sys/devices/system/cpu/online\"},\"result\":5}\n",
                "{\"event\":\"syscall\",\"syscall\":\"socket\",\"sys_nr\":41,",
                "\"args\":[2,1,0,0,0,0],\"decoded_args\":null,\"result\":4}\n",
                "{\"event\":\"syscall\",\"syscall\":\"connect\",\"sys_nr\":42,",
                "\"args\":[4,0,16,0,0,0],",
                "\"decoded_args\":{\"socket_address\":{\"family\":\"AF_INET\",",
                "\"address\":\"127.0.0.1\",\"port\":8080}},\"result\":0}\n",
                "{\"event\":\"syscall\",\"syscall\":\"keyctl\",\"sys_nr\":250,",
                "\"args\":[42,15,0,0,0,0],\"decoded_args\":null,\"result\":0}\n",
                "{\"event\":\"syscall\",\"syscall\":\"read\",\"sys_nr\":0,",
                "\"args\":[0,0,0,0,0,0],\"decoded_args\":null,\"result\":0}\n",
                "{\"event\":\"syscall\",\"syscall\":\"unknown\",\"sys_nr\":999,",
                "\"args\":[0,0,0,0,0,0],\"decoded_args\":null,\"result\":0}\n",
                "{\"event\":\"task_exit\",\"pid\":42,\"tid\":42,\"exit_code\":0,\"signal\":null}\n"
            ),
        )
        .unwrap();

        let trace_report = build_profile(&TraceOptions {
            trace_dir: traces,
            worker: "vm".to_string(),
            output_dir: temp.path().to_path_buf(),
        })
        .unwrap();

        assert_eq!(
            trace_report.policy_read_paths,
            vec![
                PathBuf::from("/sys/devices/system/cpu"),
                PathBuf::from("/usr/lib"),
            ]
        );
        assert_eq!(
            trace_report.observed_filesystem,
            vec![
                "/sys/devices/system/cpu/online (RO)",
                "/usr/lib/libexample.so (RO)",
            ]
        );
        assert_eq!(
            trace_report.observed_network,
            vec!["127.0.0.1:8080 (endpoint)", "AF_INET (address family)"]
        );
        assert_eq!(trace_report.policy_network, sandbox::Network::Loopback);
        let generated = fs_err::read_to_string(trace_report.profile_path).unwrap();
        assert!(generated.contains(".read(\"/sys/devices/system/cpu\")"));
        assert!(generated.contains(".read_exec(\"/usr/lib\")"));
        assert!(generated.contains(".network(Network::Loopback)"));
        assert!(generated.contains("Observed filesystem accesses"));
        assert!(generated.contains("Observed network accesses"));
        assert!(generated.contains("Observed syscalls"));
        assert!(
            generated.contains("Syscalls denied by policy but used by the worker:\n//! - keyctl")
        );
        assert!(!generated.contains("Configured syscall denial list"));
        assert!(!generated.contains("Generated syscall denial list"));
        assert!(generated.contains("Syscalls::deny([\n"));
        assert!(generated.contains(".name(\"vm_worker\")"));
        assert!(!generated.contains("//! - read\n"));
        assert!(!generated.contains("unknown(999)"));
        assert_eq!(
            trace_report.observed_syscalls,
            vec![
                "connect".to_string(),
                "keyctl".to_string(),
                "openat".to_string(),
                "socket".to_string()
            ]
        );
        assert!(!trace_report.policy_syscalls.contains(&"keyctl".to_string()));
    }

    #[test]
    fn mandatory_denials_remain_when_observed() {
        let mut trace_analysis = TraceAnalysis::default();
        trace_analysis.observed_syscalls.insert("kill".to_string());
        let denylist = [
            sandbox::DeniedSyscall {
                name: "kill",
                enforcement: sandbox::Enforcement::Mandatory,
                reason: None,
            },
            sandbox::DeniedSyscall {
                name: "tkill",
                enforcement: sandbox::Enforcement::Optional,
                reason: None,
            },
        ];
        trace_analysis.observed_syscalls.insert("tkill".to_string());

        assert_eq!(
            generated_syscall_denials(&denylist, &trace_analysis),
            ["kill".to_string()]
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn profile_trace_syscalls_resolve_with_sandbox_mapping() {
        let syscalls =
            pal_tracer::resolve_trace_syscalls(&["kill".to_string()], sandbox::nr_for_name)
                .unwrap();

        assert_eq!(
            syscalls.get(&libc::SYS_openat).map(String::as_str),
            Some("openat")
        );
        assert_eq!(
            syscalls.get(&libc::SYS_kill).map(String::as_str),
            Some("kill")
        );
    }

    #[test]
    fn classifies_write_and_implicit_paths() {
        let mut trace_analysis = TraceAnalysis::default();
        observe_path(
            "openat",
            [AT_FDCWD, 0, O_RDWR | O_CREAT, 0, 0, 0],
            "/var/lib/openvmm/state",
            Some(5),
            &mut trace_analysis,
        );
        observe_path(
            "newfstatat",
            [AT_FDCWD, 0, 0, 0, 0, 0],
            "/proc/self/maps",
            Some(0),
            &mut trace_analysis,
        );
        normalize_grants(&mut trace_analysis);

        assert_eq!(
            trace_analysis.policy_read_write_paths,
            BTreeSet::from([PathBuf::from("/var/lib/openvmm")])
        );
        assert!(trace_analysis.policy_read_paths.is_empty());
        assert_eq!(
            trace_analysis.observed_filesystem,
            BTreeSet::from([
                "/proc/self/maps (RO)".to_string(),
                "/var/lib/openvmm/state (RW)".to_string(),
            ])
        );
        assert!(
            trace_analysis
                .policy_filesystem_access_omissions
                .iter()
                .any(|note| note.contains("implicit /proc"))
        );
    }

    #[test]
    fn resolves_paths_through_duplicated_filesystem_fds() {
        let mut trace_analysis = TraceAnalysis::default();
        observe_path(
            "openat",
            [AT_FDCWD, 0, 0, 0, 0, 0],
            "/var/lib/openvmm",
            Some(5),
            &mut trace_analysis,
        );
        observe_fd_lifecycle("dup", [5, 0, 0, 0, 0, 0], Some(8), &mut trace_analysis);
        observe_fd_lifecycle("dup2", [8, 10, 0, 0, 0, 0], Some(10), &mut trace_analysis);
        observe_fd_lifecycle("fcntl", [10, 0, 0, 0, 0, 0], Some(12), &mut trace_analysis);
        observe_path(
            "openat",
            [12, 0, O_RDWR, 0, 0, 0],
            "state/../state/data",
            Some(9),
            &mut trace_analysis,
        );

        assert_eq!(
            trace_analysis.filesystem_fd_paths.get(&8),
            Some(&PathBuf::from("/var/lib/openvmm"))
        );
        assert_eq!(
            trace_analysis.filesystem_fd_paths.get(&10),
            Some(&PathBuf::from("/var/lib/openvmm"))
        );
        assert_eq!(
            trace_analysis.filesystem_fd_paths.get(&12),
            Some(&PathBuf::from("/var/lib/openvmm"))
        );
        assert_eq!(
            trace_analysis.filesystem_fd_paths.get(&9),
            Some(&PathBuf::from("/var/lib/openvmm/state/data"))
        );
        assert!(
            trace_analysis
                .observed_filesystem
                .contains("/var/lib/openvmm/state/data (RW)")
        );
        assert!(
            trace_analysis
                .policy_read_write_paths
                .contains(Path::new("/var/lib/openvmm/state"))
        );

        observe_fd_lifecycle("close", [12, 0, 0, 0, 0, 0], Some(0), &mut trace_analysis);
        observe_path(
            "newfstatat",
            [12, 0, 0, 0, 0, 0],
            "unresolved",
            Some(-1),
            &mut trace_analysis,
        );

        assert!(!trace_analysis.filesystem_fd_paths.contains_key(&12));
        assert!(
            trace_analysis
                .policy_filesystem_access_omissions
                .iter()
                .any(|omission| omission.contains("dirfd: 12"))
        );
    }

    #[test]
    fn generates_loopback_network_policy() {
        let mut trace_analysis = TraceAnalysis::default();
        observe_network(
            "socket",
            [AF_INET, 1, 0, 0, 0, 0],
            Some(4),
            None,
            None,
            &mut trace_analysis,
        );
        observe_network(
            "connect",
            [4, 0, 0, 0, 0, 0],
            Some(0),
            None,
            Some(&serde_json::json!({
                "socket_address": {
                    "family": "AF_INET",
                    "address": "127.0.0.1",
                    "port": 8080
                }
            })),
            &mut trace_analysis,
        );
        observe_network(
            "sendto",
            [4, 0, 0, 0, 0, 0],
            Some(12),
            None,
            None,
            &mut trace_analysis,
        );

        assert_eq!(trace_analysis.policy_network, sandbox::Network::Loopback);
    }

    #[test]
    fn generates_unrestricted_network_policy() {
        let mut trace_analysis = TraceAnalysis::default();
        observe_network(
            "connect",
            [4, 0, 0, 0, 0, 0],
            Some(0),
            None,
            Some(&serde_json::json!({
                "socket_address": {
                    "family": "AF_INET6",
                    "address": "2001:db8::1",
                    "port": 443
                }
            })),
            &mut trace_analysis,
        );

        assert_eq!(
            trace_analysis.policy_network,
            sandbox::Network::Unrestricted
        );
    }
}
