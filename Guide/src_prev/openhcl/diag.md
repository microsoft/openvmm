# Diagnosing OpenHCL

## `ohcldiag-dev`

OpenHCL includes a "diag server", which provides an interface to diagnose
and interact with the OpenHCL binary and user-mode state.

`ohcldiag-dev` is the "move-fast, break things" tool used by the core OpenHCL
dev team, and as such, it makes NO stability guarantees as to the specific
format of the CLI, output via stdout/stderr, etc...

That is to say:
**ANY AUTOMATION THAT ATTEMPTS TO USE `ohcldiag-dev` WILL EVENTUALLY BREAK!**

## `ohcldiag-dev` Examples

### Check `OpenHCL` version

You can inspect a running OpenHCL VM with ohcldiag-dev.

```powershell
PS C:\> .\ohcldiag-dev.exe inspect gen1-testvm build_info
{
   crate_name: "underhill_core",
   scm_revision: "bd7d6a98b7ca8365acdfd5fa2b10a17e62ffa766",
}
```

The detailed kernel version information is available from the initial RAM filesystem only:

```powershell
PS D:\> .\Tools\ohcldiag-dev.exe uhvm00-client run -- cat /etc/kernel-build-info.json
{
  "git_branch": "rolling-lts/underhill/5.15.90.7",
  "git_revision": "55792e0aa5e92ac4450dc10bf032caadc019fd84",
  "build_id": "74486489",
  "build_name": "5.15.90.7-hcl.1"
}

The OpenHCL version information can be read from the filesystem, too:

PS D:\> .\Tools\ohcldiag-dev.exe uhvm00-client run -- cat /etc/underhill-build-info.json
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

**Interactive shell is only available in debug OpenHCL.**

### Running a command

To run a command non-interactively:

```powershell
ohcldiag-dev.exe <vm name> run cat /proc/interrupts
```

### Using `inspect`

To inspect OpenHCL state (via the [`Inspect` trait][inspect]):

```powershell
ohcldiag-dev.exe <vm name> inspect -r
```

[inspect]: ../diag/inspect.md

### `kmsg` log

The kernel `kmsg` log currently contains both the kernel log output and the
OpenHCL log output. You can see this output via the
[console](#kernel-console), if you have it configured, or via `ohcldiag-dev`:

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
`HVLITE_LOG=<filter>`. To update it on a running VM, run:

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
ohcldiag-dev.exe myvm inspect trace/filter -u warn,storvsp=debug
```

If successful, the new filter will take effect immediately, even if you have an
open `kmsg` session already.

## Getting the build information in the debugger

Dump the contents of the `underhill_core::build_info::BUILD_INFO` global variable:

- in gdb and lldb:

```text
p underhill_core::build_info::BUILD_INFO
```

- in WinDbg:

```text
dx underhill!BUILD_INFO
```

The variable can be accessed through the non-mangled name `BUILD_INFO`, too,
in WinDbg and lldb. That does not work in gdb.

## Enable Linux Kernel Tracing

Sometimes it can be useful to extract additional information from the kernel
during runtime. By default the config OpenHCL uses does not support tracing;
as such you will need to build a custom kernel with tracing support. First, see
the [Kernel Development](kernel.md) section of the docs to find the repo. To set up
a tracing enabled kernel:

1. Find `CONFIG_FTRACE` in Microsoft/hcl-dev.config and change it from
   `CONFIG_FTRACE is not set` to `CONFIG_FTRACE=y`.
2. Build the kernel using the Microsoft/build-hcl-kernel.sh script.
3. In the loader json you intend to use (see [OpenHCL build
   instruction](build.md#building-the-openhcl-igvm-image)) change the
   `kernel_path` entry to point to your newly built vmlinux. This can
   usually be found at linux-dom0-hyperv/out/vmlinux.
4. Build OpenHCL using `cargo xflowey build-igvm --custom-kernel path/to/vmlinux`.
5. When launching your OpenHCL vm, be sure to Set-VmFirmwareParameters
   correctly. The following is an example that enables tracing hyper-v linux
   components such as vmbus: `tp_printk=1 trace_event=hyperv`
   * `tp_printk=1` tells the kernel to print traces to the kernel log.
   * `trace_events=<module>` tells the kernel which module traces to print.

## Saving the OpenHCL traces to the Windows event log on the host

The OpenHCL traces can be saved to the Windows event log on the host. That is not meant to be
a production scenario due to resource consumption concerns. By default, only ETW is emitted.

Setting `PersistentGel` under the virt. key (`HKLM:SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization`)
to `1` (`REG_DWORD`) makes the messages being stored to the host event log, too, to make getting traces
easier in the development scenarios. The traces will be stored under the Hyper-V Worker Operational log.

Here is a Powershell one-liner to enable that developer aid:

```posh
New-ItemProperty "HKLM:SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization" -Name "PersistentGel" -Value 1 -Type Dword -Force
```

*Note*: the key has to be set prior to starting the VM to save the logging messages sent by VTL2 running
inside that VM to the Windows event log.

To retrieve the events with Powershell, start with this one-liner and tweak it to your needs:

```posh
Get-WinEvent -FilterHashtable @{ LogName='Microsoft-Windows-Hyper-V-Worker-Operational'; ProviderName='Microsoft-Windows-Hyper-V-Chipset' }
```
