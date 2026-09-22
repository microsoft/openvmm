# ohcldiag-dev

OpenHCL includes a "diag server", which provides an interface to diagnose
and interact with the OpenHCL binary and user-mode state.

`ohcldiag-dev` is the "move-fast, break things" tool used by the core OpenHCL
development team. It makes no stability guarantees for command syntax,
stdout/stderr, inspect paths, or command availability.

```admonish warning title="Interactive development only"
Automation that parses or invokes `ohcldiag-dev` will eventually break. Use a
versioned management or diagnostic protocol for scripts and products.
```

`ohcldiag-dev` is designed to work no matter where you run OpenHCL: in a Hyper-V
VM, an OpenVMM VM using VSM or nested virtualization, or in other VMMs that
support paravisors. Consider the [`hypestv`][] tool for an interactive dev/test
tool specifically for Hyper-V VMs.

[`hypestv`]: ../../../dev_guide/dev_tools/hypestv.md

## Selecting a VM

The first positional argument identifies the diagnostics endpoint. Accepted
forms include:

| Form | Platform | Meaning |
| --- | --- | --- |
| `<VM_NAME>` | Windows | Hyper-V VM name, unless the value is a socket path |
| `hyperv:<VM_NAME>` | Windows | Explicit Hyper-V VM name |
| `hyperv-id:<GUID>` | Windows | Hyper-V VM identifier |
| `vsock:path/to/socket` | Windows or Linux | OpenVMM hybrid-vsock socket |
| `path/to/socket` | Windows or Linux | Hybrid-vsock socket path |

The VM must have a running OpenHCL diagnostics server and the host VMM must
expose the corresponding transport.

## Confidential VM restrictions

A production confidential VM restricts diagnostics that could reveal guest
data. Inspect exposes a reduced tree, and shell, debugger, core-dump,
saved-state, and similar operations may be unavailable.

Debug or confidential-debug images expose more functionality by design. Do not
use a debug image to infer the diagnostic surface of a production CVM.

See [CVM restrictions](./cvm_restrictions.md) for the policy details.

## Binary output safety

Commands such as `core-dump`, `perf-trace`, `dump-saved-state`, and
`memory-profile-trace` can emit binary data. Supply an output path instead of
redirecting a terminal stream where the command supports one.

Packet capture appends a NIC index to the selected output basename. Open the
result with a PCAP-compatible analyzer such as Wireshark.

## Examples

### Check `OpenHCL` version

You can inspect a running OpenHCL VM with ohcldiag-dev.

```powershell
PS > .\ohcldiag-dev.exe <vm name> inspect build_info
{
   crate_name: "underhill_core",
   scm_revision: "bd7d6a98b7ca8365acdfd5fa2b10a17e62ffa766",
}
```

Use this to validate that the VM is running the intended OpenHCL image. Compare
the reported revision with `git log --max-count=1` from the source checkout.

The detailed kernel version information is available from the initial RAM filesystem only:

```powershell
PS > .\ohcldiag-dev.exe <vm name> run -- cat /etc/kernel-build-info.json
{
  "git_branch": "rolling-lts/underhill/5.15.90.7",
  "git_revision": "55792e0aa5e92ac4450dc10bf032caadc019fd84",
  "build_id": "74486489",
  "build_name": "5.15.90.7-hcl.1"
}
```

The OpenHCL version information can be read from the filesystem, too:

```powershell
PS > .\ohcldiag-dev.exe <vm name> run -- cat /etc/underhill-build-info.json
{
    "git_branch": "user/romank/kernel_build_info",
    "git_revision": "a7c4ba3ffcd8708346d33a608f25b9287ac89f8b"
}
```

### Interactive Shell

To get an interactive shell into the VM, try:

```powershell
ohcldiag-dev.exe <vm name> shell
```

Interactive shell is only available in debug builds of OpenHCL.

### Running a command

To run a command non-interactively:

```powershell
ohcldiag-dev.exe <vm name> run cat /proc/interrupts
```

### Using `inspect`

To inspect OpenHCL state (via the `Inspect` trait):

```powershell
ohcldiag-dev.exe <vm name> inspect -r
```

### `kmsg` log

The kernel `kmsg` log currently contains both the kernel log output and the
OpenHCL log output. You can see this output via the
[serial console](./tracing.md#enabling-serial-logging-for-openhcl), when
configured, or through `ohcldiag-dev`:

```powershell
ohcldiag-dev.exe <vm name> kmsg
```

If you want a continuous stream of output as new messages arrive, pass the `-f`
flag:

```powershell
ohcldiag-dev.exe <vm name> kmsg -f
```

By default, the OpenHCL logs will only contain traces at info level and
higher. You can adjust this globally or on a module-by-module basis. And you can
set the tracing configuration at startup or dynamically with `ohcldiag-dev`.

To set the trace filter at startup, add a kernel command line option
`OPENVMM_LOG=<filter>`. To update it on a running VM, run:

```powershell
ohcldiag-dev.exe <vm name> inspect trace/filter -u <filter>
```

The format of `<filter>` is a series of comma-separated key-value pairs, plus an
optional default, `<default-level>,<target>=<level>,<target>=<level>`. `<level>`
can be one of:

* `trace`
* `debug`
* `info`
* `warn`
* `error`
* `off`

`<target>` specifies the event or span's target, which defaults to the fully
qualified module name (including the crate name) that contains the event, but it
can be overridden on individual trace statements.

So to enable warning traces by default, but debug level for storvsp traces, try:

```powershell
ohcldiag-dev.exe <vm name> inspect trace/filter -u warn,storvsp=debug
```

If successful, the new filter will take effect immediately, even if you have an
open `kmsg` session already.

**Note**: release builds have `trace` level compiled out by default, so
trace-level events cannot be enabled dynamically. To build a custom release
build with trace-level events available, you can use the `--max-trace-level`
parameter to `cargo xflowey build-igvm`:

```bash
cargo xflowey build-igvm x64 --release --max-trace-level trace
```

This is not necessary for debug builds.
