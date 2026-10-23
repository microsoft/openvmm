# sandbox

Default-deny worker-process sandboxing for OpenVMM and OpenHCL.

A worker starts from `Profile::deny_all()` and is widened only by the explicit
grants its policy declares, so confinement is default-deny by construction
rather than by remembering to lock things down. Linux is fully implemented; the
Windows LPAC launch data is produced but the worker-side half is not yet
implemented (it fails loud rather than running unconfined).

## The three stages

Sandboxing happens in three named, ordered stages:

1. **`prepare`** — runs in the *control* process, just before it spawns the
   worker. It returns a plain-data `SandboxProcessConfig` describing the
   child's launch environment (which Linux `CLONE_NEW*` flags to use, which
   handles stay inheritable, the child's identity, and — on Windows — the LPAC
   construction data). The caller passes that prepared configuration intact to
   the platform process builder, which performs the spawn. This crate depends
   on neither `mesh` nor `pal`: it decides *what* the launch environment must
   be; PAL decides *how* to apply it.
2. **`apply`** — the **first statement of the worker's `main()`**. On Linux the
   worker configures the namespaces it was cloned into, then applies
   `pivot_root`, credential drop, hardening, `no_new_privs`, and optional
   seccomp. It must run before any thread is spawned, any async runtime starts,
   or any resource is opened.
3. **`tighten`** — an optional, additive-only ratchet a worker calls once it
   has finished initializing, to shed its init-only authority. It takes
   `Restrictions`, a type that can only ever *narrow*.

## Authoring a policy

OpenVMM's default worker profiles live in dedicated modules under
`src/profiles`. Each exports a function returning `Builder`.
`SandboxRole` in `src/profiles/sandbox_profiles.rs` selects the matching builder
and calls `.build()` to return a finished `Profile`:

```rust
use sandbox::profiles::SandboxRole;

let profile = SandboxRole::Tpm.profile();
assert_eq!(profile.name(), "tpm_worker");
```

These policies are compiled into the binary. Rebuild OpenVMM after changing
them. The worker filesystem grants, network scope, and syscall denials are
defined by these linked policies.

Policies are Rust values built with a widening-only builder. Start from a
shipped base (see `sandbox::profiles`) or from `Profile::deny_all()`, then add
exactly the grants the worker needs:

```rust
use sandbox::{Network, Profile, Syscalls, profiles};

fn vtpm_profile() -> Profile {
    profiles::minimal()
        .name("vtpm")
        .read_exec("/usr/lib")
        .read_write("/var/lib/vtpm")
        .network(Network::None)
        .syscalls(Syscalls::deny([
            "execve", "socket", "connect",
        ]))
        .build()
}
```

Filesystem grants distinguish data from executable code: `.read(...)` allows
reads and directory traversal but not execution, `.read_exec(...)` also allows
execution and executable mappings for shared libraries, and `.read_write(...)`
allows reads and writes but not execution. On Linux, `.read(...)` mounts are
read-only and `noexec`; they do not remove directory search permission bits.

`Syscalls::deny` installs a curated **default-allow** seccomp filter: it always
blocks the built-in dangerous-syscall baseline (namespace-escape and kernel-CVE
vectors such as `mount`, `unshare`, `pivot_root`, and `bpf`, documented in
[`src/unix/syscall_denylist.rs`](src/unix/syscall_denylist.rs)) plus any
extra names you list, and allows everything else. A worker's full set of
*needed* syscalls can't be reliably enumerated, so the filter denies what a
sandboxed worker never legitimately needs rather than trying to allowlist what
it does.

The standalone launcher loads its additional syscall denials from the
platform configuration returned by `sandbox::platform_syscall_denylist()`.

Every builder call widens the default-deny profile only as requested:
filesystem and network methods grant exactly what is listed, and `syscalls`
installs the selected deny policy. Use `Network::Unrestricted` only for workers
that intentionally retain the caller's network namespace. Every sandbox
primitive selected by the profile is required — if the platform cannot honor
it, `apply` aborts rather than running with weaker confinement.

## Preparing the spawn (control process)

```rust
use sandbox::{HandleTag, Identity, RawHandle};

let preparation = sandbox::prepare(
    &vtpm_profile(),
    &Identity::default(),
    &[(HandleTag(0), RawHandle(log_fd as u64))],
)?;

// Merge `preparation` into your own process builder: keep
// `preparation.inherit_handles` inheritable, clone with
// `preparation.clone_flags`, honor `preparation.map_current_user`, set
// `preparation.uid` / `.gid`, and (on Windows) build the LPAC token from
// `preparation.windows`. Then exec the child.
```

Handle hygiene is the caller's job: there is no grant envelope and no
descriptor sweep inside this crate. `prepare` reports exactly which tagged
handles must remain inheritable; the caller keeps those inheritable and closes
or marks every other descriptor before the child is reached.

## Applying a policy (worker process)

`apply` is applied by the worker to itself, as the very first statement of
`main()`, before any thread is spawned:

```rust
fn main() -> anyhow::Result<()> {
    // ... only the minimal init that must run unsandboxed ...
    if let Err(e) = sandbox::apply(&vtpm_profile()) {
        eprintln!("sandbox failed: {e}");
        std::process::exit(sandbox::EXIT_SANDBOX_FAILED);
    }
    // ... everything from here runs under the sandbox ...
    Ok(())
}
```

`apply` returns `Error` if any *required* primitive fails; the worker should
then exit with `EXIT_SANDBOX_FAILED` so the control process can distinguish a
sandbox failure from an ordinary crash.

## Tightening after initialization (optional)

Once a worker has finished the init that needs broader authority, it can shed
that authority with an additive-only `Restrictions` ratchet. Unlike `apply`, it
may run multi-threaded:

```rust
use sandbox::Restrictions;

let restrictions = Restrictions::none()
    .syscalls(&["execve", "socket", "connect"])
    .build();
sandbox::tighten(&restrictions)?;
```

## Dev escape hatch

In **debug builds only**, setting the `OPENVMM_SANDBOX_DISABLE` environment
variable makes `apply` and `tighten` no-ops so a developer can run a worker
outside its confinement while iterating. Release builds ignore it entirely: the
sandbox can never be switched off by an environment variable a compromised
process could set.

## Platform support

| Platform | `prepare` | `apply` / `tighten` |
| -------- | --------- | ------------------- |
| Linux    | yes       | yes                 |
| Windows  | yes (LPAC launch data) | not yet — returns `Error::UnsupportedPlatform` |
| other    | no        | returns `Error::UnsupportedPlatform` |
