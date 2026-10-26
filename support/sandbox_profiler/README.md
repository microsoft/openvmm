# Sandbox Profiler

Without this tool, developers must identify a worker's resource requirements
and write its sandbox profile manually, which is tedious and error-prone.
`sandbox_profiler` translates recorded worker behavior into a candidate
profile by converting observed filesystem, network, and system call activity
into the corresponding sandbox policy.

Automatic profile generation has two steps. First, run OpenVMM with worker
tracing enabled. OpenVMM runs VM-related work in separate Mesh worker
processes, with each worker performing a specific function such as running a
VM or emulating a device. The tracer records the files, network endpoints, and
system calls used by the selected worker. Second, run
`sandbox_profiler` against the trace to generate the candidate Rust
sandbox profile.

## Build

Build the standalone profile-builder binary with:

```shell
cargo build -p sandbox_profiler
```

The executable is:

```text
target/debug/sandbox_profiler
```

## Collect worker traces

Worker tracing follows each separate Mesh worker with `ptrace`. It requires
separate worker processes and is not available in single-process mode, where
OpenVMM runs the control logic and all worker roles in one process.

Create a directory for the traces and pass it to OpenVMM:

```shell
mkdir -p <TRACE_DIR>

OPENVMM_SANDBOX_DISABLE=1 \
target/debug/openvmm \
    --worker-trace-dir <TRACE_DIR> \
    --processors 4 \
    --memory 2GB \
    --kernel /path/to/vmlinux \
    --initrd /path/to/initrd \
    --cmdline "console=ttyS0 single" \
    --com1 console
```

`OPENVMM_SANDBOX_DISABLE=1` disables sandbox application at runtime in debug
builds so tracing can observe the resources required by an unrestricted
worker. Release builds ignore this environment variable. Each worker writes
one `.jsonl` trace file named:

```text
worker-<worker-name>.<pid>.jsonl
```

The seccomp filter reports only profile-relevant filesystem, network, and
file-descriptor tracking system calls, plus the system calls in the platform
denylist, to `ptrace`. Other system calls execute normally without being
intercepted, which reduces the performance impact on the worker. OpenVMM
validates every configured denial before launching a traced worker and stops
with an error if tracing support for a configured name is missing.

## Generate a profile

Run the standalone profile-builder binary against the traces collected in the
previous step:

```shell
target/debug/sandbox_profiler \
    <TRACE_DIR> <WORKER> \
    [--output-dir <OUTPUT_DIR>]
```

`<TRACE_DIR>` is the directory created in the trace-collection step. The
profile builder searches this directory and its subdirectories for matching
trace files.

`<WORKER>` is the worker name from
`worker-<worker-name>.<pid>.jsonl`.

`--output-dir <OUTPUT_DIR>` is optional. Without it, the profile builder writes
the generated profile to `<TRACE_DIR>/<WORKER>_worker.rs`.

The profile builder uses the platform denylist exposed by the `sandbox`
crate.

The generated profile is a starting point and must be reviewed before it is
integrated into the worker.

#### Filesystem policy

The profile builder uses the file accesses in the trace to determine which
directories the worker needs to read or modify. Read access under `/sys`
produces `.read(...)` entries, which allow reads and directory traversal but
not execution. Other read access conservatively produces `.read_exec(...)`
entries because the trace does not capture executable mappings of shared
libraries. Review these grants and use `.read(...)` for data that does not
need execution. Write access produces non-executable `.read_write(...)` entries.
`/proc` and `/tmp` are provided by the sandbox and are described in the
generated profile's filesystem notes instead of being added as directory
entries.

#### Network policy

The profile builder generates the narrowest network policy supported by the
sandbox that covers the successful IP activity in the trace. It generates
`Network::None` when no IP communication is observed, `Network::Loopback` when
all observed endpoints are loopback addresses, and `Network::Unrestricted`
when an external endpoint is observed or the destination cannot be determined.
Because unrestricted networking retains the caller's network namespace, review
that result before integrating the generated profile.

#### System call policy

The profile builder reads the platform system call denylist from the
configuration file selected by the `sandbox` crate. It removes entries
observed in the worker trace and writes the remaining denials to the generated
profile. It validates the configured names before reading the trace files and
stops with an error if tracing support for a configured name is missing.

The sandbox also applies its built-in mandatory syscall restrictions whenever
system call filtering is enabled. They are defined in
`support/sandbox/src/unix/seccomp.rs` and are maintained by the sandbox
implementation rather than duplicated here.
