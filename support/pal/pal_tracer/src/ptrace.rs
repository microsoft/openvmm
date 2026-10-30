// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Linux ptrace collection for child processes.

// UNSAFETY: ptrace, waitpid, kill, and process_vm_readv are Linux process APIs.
#![expect(unsafe_code)]

use crate::TraceConfig;
use seccompiler::SeccompAction;
use seccompiler::SeccompFilter;
use seccompiler::TargetArch;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::io::BufWriter;
use std::io::Write;
use std::mem::MaybeUninit;
use std::mem::size_of;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

const MAX_STRING_LEN: usize = 16 * 1024;
const TRACE_BUFFER_CAPACITY: usize = 256 * 1024;

// Linux UAPI bindings from include/uapi/linux/ptrace.h:
// https://github.com/torvalds/linux/blob/v6.12/include/uapi/linux/ptrace.h
// libc 0.2.184 exposes these on GNU but not musl; the kernel ABI is the same.
const PTRACE_GET_SYSCALL_INFO: libc::c_uint = 0x420e;
const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;
const PTRACE_SYSCALL_INFO_SECCOMP: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy)]
struct PtraceSyscallInfo {
    op: u8,
    pad: [u8; 3],
    arch: u32,
    instruction_pointer: u64,
    stack_pointer: u64,
    u: PtraceSyscallInfoData,
}

#[repr(C)]
#[derive(Clone, Copy)]
union PtraceSyscallInfoData {
    entry: PtraceSyscallInfoEntry,
    exit: PtraceSyscallInfoExit,
    seccomp: PtraceSyscallInfoSeccomp,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PtraceSyscallInfoEntry {
    nr: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PtraceSyscallInfoExit {
    sval: i64,
    is_error: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PtraceSyscallInfoSeccomp {
    nr: u64,
    args: [u64; 6],
    ret_data: u32,
}

/// Build the seccomp filter that stops profile-relevant syscalls under ptrace.
pub fn build_seccomp_filter(config: &TraceConfig) -> io::Result<SeccompFilter> {
    // Matching syscalls stop under ptrace, while every other syscall continues normally.
    let rules = config
        .syscalls
        .keys()
        .copied()
        .map(|sys_nr| (sys_nr as libc::c_long, Vec::new()))
        .collect::<BTreeMap<_, _>>();
    SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Trace(0),
        target_arch(),
    )
    .map_err(|error| io::Error::other(format!("seccomp filter creation failed: {error}")))
}

fn target_arch() -> TargetArch {
    #[cfg(target_arch = "x86_64")]
    {
        TargetArch::x86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        TargetArch::aarch64
    }
}

/// Represents a child running under ptrace-based syscall tracing.
///
/// [`TracedChild::start`] moves the child process to a dedicated thread that
/// performs the blocking ptrace work. [`TracedChild::wait`] receives the
/// status sent by that thread after tracing finishes.
pub struct TracedChild {
    completion: Option<async_channel::Receiver<io::Result<ExitStatus>>>,
}

impl TracedChild {
    /// Starts a dedicated ptrace thread before the child begins executing.
    pub fn start(
        child: pal::unix::process::Child,
        process_name: &str,
        config: TraceConfig,
    ) -> io::Result<Self> {
        let pid = child.id();
        let process_name = process_name.to_owned();
        let (send, recv) = async_channel::bounded(1);
        // ptrace is driven by blocking waitpid calls. Keep that state machine
        // on a dedicated thread and expose only its completion asynchronously.
        std::thread::Builder::new()
            .name(format!("ptrace-{process_name}-{pid}"))
            .spawn(move || {
                let result = trace_process(child, &process_name, &config);
                let _ = send.send_blocking(result);
            })?;
        Ok(Self {
            completion: Some(recv),
        })
    }

    /// Waits for the root child process and its tracer to finish.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.completion
            .take()
            .expect("traced child waited more than once")
            .recv()
            .await
            .map_err(|_| io::Error::other("process tracer exited without reporting status"))?
    }
}

// Seccomp stops a thread before the selected syscall runs, so its arguments
// are available but its result is not. Keep this entry information for that
// thread until ptrace stops it again after the syscall returns.
struct PendingSyscall {
    sys_nr: u64,
    name: String,
    args: [u64; 6],
    started: Instant,
    decoded_args: Value,
}

// Runs the tracing loop for one child process tree: attaches before exec,
// follows new tasks, pairs selected syscall entries with exits, and serializes
// the resulting activity as JSONL.
fn trace_process(
    child: pal::unix::process::Child,
    process_name: &str,
    config: &TraceConfig,
) -> io::Result<ExitStatus> {
    let root_pid = child.id();
    fs_err::create_dir_all(&config.output_dir)?;
    let trace_path = config.output_dir.join(format!(
        "{}-{}.{root_pid}.jsonl",
        sanitize_name(&config.file_prefix),
        sanitize_name(process_name)
    ));
    // Batch writes so a slow output filesystem cannot stall a traced task at
    // every syscall stop. The buffer is flushed when it fills and at exit.
    let mut output = BufWriter::with_capacity(TRACE_BUFFER_CAPACITY, File::create(&trace_path)?);

    wait_for_initial_stop(root_pid)?;
    ptrace(
        libc::PTRACE_SEIZE as _,
        root_pid,
        0,
        trace_options() as usize,
    )?;
    // Release the pre-exec group stop. PTRACE_O_TRACEEXEC guarantees another
    // stop before the child's new image executes user code.
    // SAFETY: kill accepts any PID and signal value; errors are reported via errno.
    syscall_result(unsafe { libc::kill(root_pid, libc::SIGCONT) })?;

    write_event(
        &mut output,
        json!({
            "event": "trace_started",
            "pid": root_pid,
            "trace_file": trace_path,
        }),
    )?;

    // ptrace reports stops for every followed thread and descendant through
    // one wait queue. The optional value tracks whether that task is between a
    // selected syscall's entry and exit stops.
    let mut tasks = HashMap::<i32, Option<PendingSyscall>>::new();
    tasks.insert(root_pid, None);

    loop {
        let mut status = 0;
        // SAFETY: status points to writable memory, and the options are valid waitpid flags.
        let tid = syscall_result(unsafe {
            libc::waitpid(-1, &mut status, libc::__WALL | libc::__WNOTHREAD)
        })?;

        if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
            write_event(
                &mut output,
                json!({
                    "event": "task_exit",
                    "pid": root_pid,
                    "tid": tid,
                    "exit_code": libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
                    "signal": libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status)),
                }),
            )?;
            tasks.remove(&tid);
            if tid == root_pid {
                output.flush()?;
                drop(child);
                return Ok(ExitStatus::from_raw(status));
            }
            continue;
        }

        if !libc::WIFSTOPPED(status) {
            continue;
        }

        let signal = libc::WSTOPSIG(status);
        let event = status >> 16;
        // Only a seccomp-selected entry switches a task to PTRACE_SYSCALL.
        // All other tasks run with PTRACE_CONT so unrelated syscalls do not
        // generate entry/exit stops.
        if signal == (libc::SIGTRAP | 0x80) {
            handle_syscall_exit(&mut output, root_pid, tid, tasks.entry(tid).or_default())?;
            resume_cont(tid, 0)?;
        } else if signal == libc::SIGTRAP && event == libc::PTRACE_EVENT_SECCOMP {
            handle_seccomp_stop(tid, tasks.entry(tid).or_default(), &config.syscalls)?;
            resume_syscall(tid, 0)?;
        } else if signal == libc::SIGTRAP && event != 0 {
            handle_ptrace_event(&mut output, root_pid, tid, event, &mut tasks)?;
            resume_task(tid, tasks.get(&tid).is_some_and(Option::is_some), 0)?;
        } else {
            // Synthetic SIGTRAP stops were handled above; deliver real traps.
            let deliver = if signal == libc::SIGSTOP { 0 } else { signal };
            resume_task(tid, tasks.get(&tid).is_some_and(Option::is_some), deliver)?;
        }
    }
}

fn wait_for_initial_stop(pid: i32) -> io::Result<()> {
    let mut status = 0;
    // SAFETY: status points to writable memory, and WUNTRACED is a valid waitpid flag.
    syscall_result(unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) })?;
    if !libc::WIFSTOPPED(status) || libc::WSTOPSIG(status) != libc::SIGSTOP {
        return Err(io::Error::other(format!(
            "child {pid} did not stop before exec: status {status:#x}"
        )));
    }
    Ok(())
}

// Records process-tree changes reported by ptrace and enrolls newly created
// tasks in the per-thread syscall state machine.
fn handle_ptrace_event(
    output: &mut BufWriter<File>,
    root_pid: i32,
    tid: i32,
    event: i32,
    tasks: &mut HashMap<i32, Option<PendingSyscall>>,
) -> io::Result<()> {
    let event_name = match event {
        libc::PTRACE_EVENT_FORK => "fork",
        libc::PTRACE_EVENT_VFORK => "vfork",
        libc::PTRACE_EVENT_CLONE => "clone",
        libc::PTRACE_EVENT_EXEC => "exec",
        libc::PTRACE_EVENT_EXIT => "exit",
        _ => "other",
    };

    let mut new_tid = None;
    if matches!(
        event,
        libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE
    ) {
        let mut message = 0usize;
        ptrace(
            libc::PTRACE_GETEVENTMSG as _,
            tid,
            0,
            std::ptr::from_mut(&mut message) as usize,
        )?;
        let child_tid = i32::try_from(message)
            .map_err(|_| io::Error::other("ptrace returned an invalid child tid"))?;
        // The ptrace follow options attach descendants automatically; adding
        // the new task here gives it independent syscall entry/exit state.
        tasks.insert(child_tid, None);
        new_tid = Some(child_tid);
    }

    write_event(
        output,
        json!({
            "event": "ptrace_event",
            "pid": root_pid,
            "tid": tid,
            "kind": event_name,
            "new_tid": new_tid,
        }),
    )
}

// Captures a selected syscall at entry, including argument data that must be
// read from the tracee before its address space can change.
fn handle_seccomp_stop(
    tid: i32,
    pending: &mut Option<PendingSyscall>,
    traced_syscall_map: &BTreeMap<libc::c_long, String>,
) -> io::Result<()> {
    let mut info = MaybeUninit::<PtraceSyscallInfo>::zeroed();
    let size = size_of::<PtraceSyscallInfo>();
    ptrace(
        PTRACE_GET_SYSCALL_INFO,
        tid,
        size,
        info.as_mut_ptr() as usize,
    )?;
    // SAFETY: the kernel initialized the structure after a successful ptrace.
    let info = unsafe { info.assume_init() };

    if info.op != PTRACE_SYSCALL_INFO_SECCOMP {
        return Err(io::Error::other(format!(
            "unexpected syscall info operation {} at seccomp stop",
            info.op
        )));
    }

    // SAFETY: op identifies the active union member.
    let entry = unsafe { info.u.seccomp };
    let sys_nr = entry.nr;
    let name = traced_syscall_map
        .get(&(sys_nr as libc::c_long))
        .ok_or_else(|| {
            io::Error::other(format!(
                "seccomp reported non-allowlisted syscall number {sys_nr}"
            ))
        })?;
    // Decode pointer arguments while the task is stopped at entry. The child
    // may change or unmap the referenced memory before the syscall returns.
    *pending = Some(PendingSyscall {
        sys_nr,
        name: name.clone(),
        args: entry.args,
        started: Instant::now(),
        decoded_args: decode_args(tid, sys_nr, entry.args),
    });
    Ok(())
}

// Completes the syscall record with its return value, error, and elapsed time,
// then clears the per-thread entry state.
fn handle_syscall_exit(
    output: &mut BufWriter<File>,
    root_pid: i32,
    tid: i32,
    pending: &mut Option<PendingSyscall>,
) -> io::Result<()> {
    let mut info = MaybeUninit::<PtraceSyscallInfo>::zeroed();
    let size = size_of::<PtraceSyscallInfo>();
    ptrace(
        PTRACE_GET_SYSCALL_INFO,
        tid,
        size,
        info.as_mut_ptr() as usize,
    )?;
    // SAFETY: the kernel initialized the structure after a successful ptrace.
    let info = unsafe { info.assume_init() };
    if info.op != PTRACE_SYSCALL_INFO_EXIT {
        return Err(io::Error::other(format!(
            "unexpected syscall info operation {} at syscall exit stop",
            info.op
        )));
    }

    // SAFETY: op identifies the active union member.
    let exit = unsafe { info.u.exit };
    let entry = pending
        .take()
        .ok_or_else(|| io::Error::other("syscall exit stop has no pending seccomp event"))?;
    write_event(
        output,
        json!({
            "event": "syscall",
            "timestamp_ns": unix_timestamp_ns(),
            "pid": root_pid,
            "tid": tid,
            "syscall": entry.name,
            "sys_nr": entry.sys_nr,
            "args": entry.args,
            "decoded_args": entry.decoded_args,
            "result": exit.sval,
            "errno": (exit.is_error != 0).then(|| -exit.sval),
            "duration_ns": entry.started.elapsed().as_nanos(),
        }),
    )?;
    Ok(())
}

// Converts policy-relevant pointer arguments into stable trace data. Keeping
// this decoding in the tracer lets the profile builder operate entirely on
// JSON without reading child memory.
fn decode_args(tid: i32, sys_nr: u64, args: [u64; 6]) -> Value {
    // Raw register arguments are always emitted. Decode only data needed for
    // filesystem and network policy generation; unreadable tracee memory is
    // represented by a null decoded value rather than aborting collection.
    if sys_nr == libc::SYS_renameat2 as u64 {
        return json!({
            "path": read_c_string(tid, args[1]).ok(),
            "path2": read_c_string(tid, args[3]).ok(),
        });
    }

    let path_address = if sys_nr == libc::SYS_openat as u64
        || sys_nr == libc::SYS_newfstatat as u64
        || sys_nr == libc::SYS_statx as u64
        || sys_nr == libc::SYS_readlinkat as u64
        || sys_nr == libc::SYS_faccessat as u64
        || sys_nr == libc::SYS_faccessat2 as u64
        || sys_nr == libc::SYS_mkdirat as u64
        || sys_nr == libc::SYS_unlinkat as u64
    {
        Some(args[1])
    } else {
        None
    };

    if let Some(path) = path_address.and_then(|address| read_c_string(tid, address).ok()) {
        return json!({ "path": path });
    }

    let socket_address = if sys_nr == libc::SYS_bind as u64 || sys_nr == libc::SYS_connect as u64 {
        Some((args[1], args[2]))
    } else if sys_nr == libc::SYS_sendto as u64 {
        Some((args[4], args[5]))
    } else {
        None
    };
    if let Some((address, length)) = socket_address {
        return decode_socket_address(tid, address, length).unwrap_or(Value::Null);
    }

    Value::Null
}

fn decode_socket_address(tid: i32, address: u64, length: u64) -> io::Result<Value> {
    // The length originates in the tracee. sockaddr_storage bounds the read
    // while still covering every socket address representation decoded here.
    let length = usize::try_from(length)
        .unwrap_or(usize::MAX)
        .min(size_of::<libc::sockaddr_storage>());
    let bytes = read_process_memory(tid, address, length)?;
    parse_socket_address(&bytes)
        .map(|(family, address, port)| {
            json!({
                "socket_address": {
                    "family": family,
                    "address": address,
                    "port": port,
                }
            })
        })
        .ok_or_else(|| io::Error::other("unsupported or truncated socket address"))
}

fn parse_socket_address(bytes: &[u8]) -> Option<(&'static str, String, u16)> {
    let family = u16::from_ne_bytes(bytes.get(..2)?.try_into().ok()?);
    if family == libc::AF_INET as u16 {
        let port = u16::from_be_bytes(bytes.get(2..4)?.try_into().ok()?);
        let address = Ipv4Addr::new(
            *bytes.get(4)?,
            *bytes.get(5)?,
            *bytes.get(6)?,
            *bytes.get(7)?,
        );
        Some(("AF_INET", address.to_string(), port))
    } else if family == libc::AF_INET6 as u16 {
        let port = u16::from_be_bytes(bytes.get(2..4)?.try_into().ok()?);
        let address = Ipv6Addr::from(<[u8; 16]>::try_from(bytes.get(8..24)?).ok()?);
        Some(("AF_INET6", address.to_string(), port))
    } else {
        None
    }
}

fn read_c_string(tid: i32, address: u64) -> io::Result<String> {
    // Trace inputs are not trusted to contain a terminator. Bound the snapshot
    // so malformed pointers cannot cause an unbounded allocation or read.
    let mut bytes = read_process_memory(tid, address, MAX_STRING_LEN)?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    bytes.truncate(end);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

// Copies argument data from a stopped tracee without changing its registers or
// injecting operations into the child.
fn read_process_memory(tid: i32, address: u64, length: usize) -> io::Result<Vec<u8>> {
    if address == 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "null address"));
    }
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "zero-length read",
        ));
    }

    let mut bytes = vec![0u8; length];
    let local = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let remote = libc::iovec {
        iov_base: address as usize as *mut c_void,
        iov_len: bytes.len(),
    };
    // The tracee is stopped, so this captures a stable snapshot without
    // injecting helper code or changing the child's address space.
    // SAFETY: local describes the writable byte buffer, and remote is interpreted
    // by the kernel in the tracee's address space.
    let count = syscall_result(unsafe { libc::process_vm_readv(tid, &local, 1, &remote, 1, 0) })?;
    bytes.truncate(count as usize);
    Ok(bytes)
}

fn trace_options() -> libc::c_long {
    // Follow all process/thread creation so activity cannot escape the child
    // trace, and terminate tracees if the tracer itself unexpectedly exits.
    (libc::PTRACE_O_TRACESYSGOOD
        | libc::PTRACE_O_TRACEFORK
        | libc::PTRACE_O_TRACEVFORK
        | libc::PTRACE_O_TRACECLONE
        | libc::PTRACE_O_TRACEEXEC
        | libc::PTRACE_O_TRACEEXIT
        | libc::PTRACE_O_TRACESECCOMP
        | libc::PTRACE_O_EXITKILL) as libc::c_long
}

fn resume_task(tid: i32, awaiting_syscall_exit: bool, signal: i32) -> io::Result<()> {
    // PTRACE_SYSCALL is used only long enough to collect the return value for a
    // selected syscall; otherwise the task runs until its next real event.
    if awaiting_syscall_exit {
        resume_syscall(tid, signal)
    } else {
        resume_cont(tid, signal)
    }
}

fn resume_cont(tid: i32, signal: i32) -> io::Result<()> {
    resume_ptrace(libc::PTRACE_CONT as _, tid, signal)
}

fn resume_syscall(tid: i32, signal: i32) -> io::Result<()> {
    resume_ptrace(libc::PTRACE_SYSCALL as _, tid, signal)
}

fn resume_ptrace(request: libc::c_uint, tid: i32, signal: i32) -> io::Result<()> {
    // A task may exit between waitpid reporting its stop and this resume.
    // Treat that race as normal completion; all other ptrace errors are fatal.
    match ptrace(request, tid, 0, signal as usize) {
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        result => result,
    }
}

fn ptrace(request: libc::c_uint, pid: i32, address: usize, data: usize) -> io::Result<()> {
    // GNU takes c_uint requests, musl takes c_int. All requests used here fit
    // both types; normalize to c_uint internally and cast at the libc boundary.
    // SAFETY: each caller supplies the address and data layout required by the
    // specific ptrace request; kernel errors are returned through errno.
    let result = unsafe {
        libc::ptrace(
            request as _,
            pid,
            address as *mut c_void,
            data as *mut c_void,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn syscall_result<T>(result: T) -> io::Result<T>
where
    T: Copy + PartialOrd + From<i8>,
{
    if result < T::from(0) {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

fn write_event(output: &mut BufWriter<File>, event: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, &event)?;
    output.write_all(b"\n")
}

fn unix_timestamp_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::mem::align_of;
    use std::mem::offset_of;
    use test_with_tracing::test;

    #[test]
    fn ptrace_syscall_info_matches_linux_abi() {
        assert_eq!(size_of::<PtraceSyscallInfo>(), 88);
        assert_eq!(align_of::<PtraceSyscallInfo>(), 8);
        assert_eq!(offset_of!(PtraceSyscallInfo, op), 0);
        assert_eq!(offset_of!(PtraceSyscallInfo, arch), 4);
        assert_eq!(offset_of!(PtraceSyscallInfo, instruction_pointer), 8);
        assert_eq!(offset_of!(PtraceSyscallInfo, stack_pointer), 16);
        assert_eq!(offset_of!(PtraceSyscallInfo, u), 24);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.entry.nr), 24);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.entry.args), 32);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.exit.sval), 24);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.exit.is_error), 32);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.seccomp.nr), 24);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.seccomp.args), 32);
        assert_eq!(offset_of!(PtraceSyscallInfo, u.seccomp.ret_data), 80);
    }

    #[cfg(target_env = "gnu")]
    #[test]
    fn ptrace_syscall_info_matches_libc() {
        assert_eq!(PTRACE_GET_SYSCALL_INFO, libc::PTRACE_GET_SYSCALL_INFO);
        assert_eq!(PTRACE_SYSCALL_INFO_EXIT, libc::PTRACE_SYSCALL_INFO_EXIT);
        assert_eq!(
            PTRACE_SYSCALL_INFO_SECCOMP,
            libc::PTRACE_SYSCALL_INFO_SECCOMP
        );
        assert_eq!(
            size_of::<PtraceSyscallInfo>(),
            size_of::<libc::ptrace_syscall_info>()
        );
        assert_eq!(
            align_of::<PtraceSyscallInfo>(),
            align_of::<libc::ptrace_syscall_info>()
        );

        macro_rules! assert_field_offset {
            ($($field:ident).+) => {
                assert_eq!(
                    offset_of!(PtraceSyscallInfo, $($field).+),
                    offset_of!(libc::ptrace_syscall_info, $($field).+),
                    stringify!($($field).+),
                );
            };
        }

        assert_field_offset!(op);
        assert_field_offset!(arch);
        assert_field_offset!(instruction_pointer);
        assert_field_offset!(stack_pointer);
        assert_field_offset!(u);
        assert_field_offset!(u.entry.nr);
        assert_field_offset!(u.entry.args);
        assert_field_offset!(u.exit.sval);
        assert_field_offset!(u.exit.is_error);
        assert_field_offset!(u.seccomp.nr);
        assert_field_offset!(u.seccomp.args);
        assert_field_offset!(u.seccomp.ret_data);
    }

    #[test]
    fn process_names_are_safe_for_file_names() {
        assert_eq!(sanitize_name("vm"), "vm");
        assert_eq!(sanitize_name("worker/name"), "worker_name");
    }

    #[test]
    fn parses_ipv4_socket_address() {
        let mut bytes = [0u8; 8];
        bytes[..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
        bytes[2..4].copy_from_slice(&8080u16.to_be_bytes());
        bytes[4..].copy_from_slice(&[127, 0, 0, 1]);

        assert_eq!(
            parse_socket_address(&bytes),
            Some(("AF_INET", "127.0.0.1".to_string(), 8080))
        );
    }

    #[test]
    fn parses_ipv6_socket_address() {
        let mut bytes = [0u8; 24];
        bytes[..2].copy_from_slice(&(libc::AF_INET6 as u16).to_ne_bytes());
        bytes[2..4].copy_from_slice(&443u16.to_be_bytes());
        bytes[23] = 1;

        assert_eq!(
            parse_socket_address(&bytes),
            Some(("AF_INET6", "::1".to_string(), 443))
        );
    }

    #[test]
    fn traces_process_to_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let config = TraceConfig::new(
            dir.path().to_path_buf(),
            "worker",
            BTreeMap::from([(libc::SYS_openat, "openat".to_string())]),
        );
        let mut command = pal::unix::process::Builder::new("/usr/bin/true");
        command.set_trace_seccomp_filter(build_seccomp_filter(&config).unwrap());
        let child = command.spawn().unwrap();
        let pid = child.id();
        let mut traced = TracedChild::start(child, "test-worker", config).unwrap();

        let status = futures::executor::block_on(traced.wait()).unwrap();
        assert!(status.success());

        let file = File::open(dir.path().join(format!("worker-test-worker.{pid}.jsonl"))).unwrap();
        let events = io::BufReader::new(file)
            .lines()
            .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap())
            .collect::<Vec<_>>();
        assert!(
            events
                .iter()
                .all(|event| event.get("version").is_none() && event.get("worker").is_none())
        );
        assert!(events.iter().any(|event| event["event"] == "trace_started"));
        assert!(events.iter().any(|event| {
            event["event"] == "syscall"
                && event["sys_nr"].is_number()
                && event.get("decoded_args").is_some()
                && event.get("number").is_none()
                && event.get("details").is_none()
        }));
        assert!(
            events
                .iter()
                .filter(|event| event["event"] == "syscall")
                .all(|event| {
                    let sys_nr = event["sys_nr"].as_u64().unwrap();
                    sys_nr == libc::SYS_openat as u64 && event["syscall"] == "openat"
                })
        );
        assert!(events.iter().any(|event| event["event"] == "task_exit"));
    }

    #[test]
    fn delivers_sigtrap_to_handler() {
        let status = trace_shell("trap 'exit 42' TRAP; kill -TRAP $$; exit 0");
        assert_eq!(status.code(), Some(42), "{status}");
    }

    #[test]
    fn preserves_unhandled_sigtrap() {
        let status = trace_shell("ulimit -c 0; trap - TRAP; kill -TRAP $$; exit 0");
        assert_eq!(status.signal(), Some(libc::SIGTRAP), "{status}");
    }

    fn trace_shell(script: &str) -> ExitStatus {
        let dir = tempfile::tempdir().unwrap();
        let config = TraceConfig::new(
            dir.path().to_path_buf(),
            "worker",
            BTreeMap::from([(libc::SYS_openat, "openat".to_string())]),
        );
        let mut command = pal::unix::process::Builder::new("/bin/sh");
        command
            .args(["-c", script])
            .set_trace_seccomp_filter(build_seccomp_filter(&config).unwrap());
        let child = command.spawn().unwrap();
        let mut traced = TracedChild::start(child, "test-worker", config).unwrap();
        futures::executor::block_on(traced.wait()).unwrap()
    }
}
