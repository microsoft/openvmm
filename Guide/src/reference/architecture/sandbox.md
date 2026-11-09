# Worker sandboxing

The [`sandbox`](https://openvmm.dev/rustdoc/linux/sandbox/index.html) crate
provides process-confinement mechanisms for worker hosts. It separates the
policy describing a worker's authority from the process-launch code that
establishes it.

**Integration status:** Linux confinement and optional PAL/Mesh launch support
are implemented. Existing OpenVMM workers still use ordinary launches; this
infrastructure does not enable sandboxing automatically. Windows preparation
data exists, but neither PAL's sandboxed Windows launch nor the worker-side
backend is implemented. Requesting those paths returns an error.

## Scope and trust boundary

The purpose is to limit what a compromised worker can reach, not to prevent
worker compromise or replace validation of guest input. The control process
retains ambient authority and supplies the worker's resources.

The unit of confinement is an OS process, not an individual Rust worker. A
[Mesh host](openvmm/mesh/usage.md#workers) can run multiple workers, but all
workers and the Mesh runtime in that process share its sandbox. Workers that
need different authority must use separate host processes.

Resources already held by the worker remain authority. An inherited descriptor
or a device handle delivered through Mesh is not made harmless by restricting
filesystem paths. Likewise, Mesh messages from a confined worker remain
untrusted and must be validated by their recipients.

## Policy and mechanism

- **`sandbox::Profile` and `Builder`** describe filesystem grants, network
  scope, and optional syscall denials.
- **The `sandbox` platform backend** translates that policy into OS
  restrictions on the current process.
- **`sandbox::profiles`** supplies concrete OpenVMM VM/TPM policy presets and
  shared bases. These are consumer policy, not requirements of every sandbox.
- **The PAL process builder** creates the child with the prepared namespace
  configuration, maps IDs, and enforces descriptor inheritance.
- **`mesh_process`** carries an optional profile through preparation and
  preserves the Mesh bootstrap transport, without selecting worker roles.
- **The consumer entry point** selects a matching policy for launch and worker
  startup, applies it before worker initialization, and terminates on failure.

Profiles are Rust values compiled into the program. Mesh does not transmit a
profile to the child: the worker applies its linked policy. Launch preparation
and worker-side policy must agree.

The VM and TPM presets are not a universal definition of safe worker
authority. For example, the VM preset retains the caller's network namespace
and grants selected device nodes; the TPM preset uses an isolated network
namespace. Their filesystem choices belong beside the
[profile definitions](https://github.com/microsoft/openvmm/tree/main/support/sandbox/src/profiles),
not in the generic process launcher.

## Launch lifecycle

There are three API stages and a small pre-exec step in the process builder:

1. **Prepare, in the control process.** `sandbox::prepare` returns inert
   `SandboxProcessConfig` data. It does not create a process, change handles,
   or apply confinement to the caller.
2. **Spawn, in PAL.** On Linux, PAL creates the requested namespaces, writes
   the child's UID/GID mappings, preserves explicitly selected descriptors,
   and executes the worker image.
3. **Apply, in the worker.** `sandbox::apply` assembles the filesystem view
   and applies process restrictions before joining Mesh, starting threads, or
   consuming untrusted input.
4. **Tighten, optionally after initialization.** `sandbox::tighten` stacks
   additional restrictions. Linux currently supports additional seccomp
   denials synchronized across all threads; path revocation is unsupported.

PAL preserves its vfork-based launch path. Its pre-exec callback shares the
parent's address space and must not allocate, lock, trace, or construct policy.
Namespace mappings use precomputed data and libc operations. Rich setup belongs
after `execve`, in the fresh, single-threaded worker image.

The Linux sandbox is not fully established at process creation. The loader,
runtime startup, and code through successful `apply` are trusted bootstrap.
They must not start worker threads, join Mesh, or process attacker-controlled
input before confinement completes.

The bootstrap Mesh descriptor survives launch filtering. After `apply`,
normal working resources can arrive through Mesh's typed messages and
[OS-resource transport](openvmm/mesh/internals.md#joining-a-mesh).

## Linux confinement

The primary filesystem boundary is a new mount namespace containing a tmpfs
root populated by explicit bind mounts. The worker pivots into that root and
detaches the old root. Grant-specific mount attributes distinguish readable
data, executable code, writable data, and single-device access.

User, mount, and IPC namespaces are required. The user namespace allows setup
without requiring host root privileges, but does not allocate a distinct host
identity: namespace UID/GID 0 map to the spawning process's effective UID/GID.
A new network namespace is also required unless the profile selects
`Network::Unrestricted`; loopback can be enabled within an isolated namespace.

Landlock supplements the mount boundary where available. Its rules degrade
according to the kernel ABI, and failure logs a warning rather than aborting
startup. It is not the primary filesystem boundary. The filesystem setup also
provides scratch storage and a best-effort proc view; a profile with no
explicit grants is not a process with no filesystem objects.

After filesystem setup, the backend locks securebits, drops capabilities,
disables dumpability, and sets `no_new_privs`. Seccomp, when selected by the
profile, is installed last so it can deny setup syscalls that are no longer
needed. It is a default-allow denylist, not a complete syscall allowlist.
Selecting no seccomp policy leaves that syscall surface unfiltered.

Required setup failures are returned to the caller; launch is not retried with
fewer namespaces. Worker-side setup can fail after irreversible restrictions
have been applied, so the caller must terminate rather than continue
initialization. The crate provides `EXIT_SANDBOX_FAILED` for that purpose.

## Availability and limits

Linux availability depends on both kernel support and host policy permitting
the requested namespace operations and UID/GID mappings. Unprivileged setup
does not bypass AppArmor or container restrictions. The crate does not install
host policy or change system security settings. See the
[platform prerequisites and test instructions](https://github.com/microsoft/openvmm/tree/main/support/sandbox#platform-support)
for the Ubuntu AppArmor requirement and opt-in namespace tests.

Linux workers currently retain their host UID/GID and PID visibility, and
cross-process signaling is not yet restricted. This is not a complete
isolation boundary against other processes running under the same host UID.
Passed device handles also retain their underlying kernel interface surface.

Windows launch-policy types describe an intended LPAC configuration, not an
implemented Windows isolation boundary. Future platform support or additional
restrictions must be validated independently; they are not guarantees of the
current backend.
