# Debugging OpenHCL

OpenHCL provides several debugging tools for investigating issues at the
user-mode and kernel level. See [ohcldiag-dev](./diag/ohcldiag_dev.md) for the
diagnostic client and [Tracing](./diag/tracing.md) for serial and event log
tracing.

## On-demand memory dumps

Use `ohcldiag-dev dump` to capture a live user-mode memory dump of the OpenHCL
process at any time. The dump is an ELF core file that you can analyze with
`lldb`, `gdb`, or `rust-lldb`. See the
[ohcldiag-dev](./diag/ohcldiag_dev.md) page for the full command reference.

## User-mode crash dumps

When an OpenHCL user-mode process crashes, a crash dump is automatically
generated via the `underhill-crash` infrastructure and sent to the host over
VMBus. On Windows hosts, these dumps are collected by Windows Error Reporting
(WER). Use `lldb` or `gdb` to analyze the resulting ELF core dump.

## Kernel crash dumps

Kernel-mode crash dumps (kdump) are **not currently supported** in OpenHCL. The
OpenHCL kernel does not have `CONFIG_KDUMP` or `CONFIG_KEXEC` compiled in. If
the kernel panics, no dump is generated. The only diagnostic output from a kernel
panic is serial console output (if COM3 is enabled) or whatever was captured by
`ohcldiag-dev` before the panic.

For debugging kernel-level issues, the best approach is to enable serial output
via COM3 (see below) — it captures output from the very first instruction of
kernel boot.

## Getting OpenHCL kernel logs (COM3 vs ohcldiag-dev)

Two methods exist for capturing OpenHCL kernel (`kmsg`) output. They differ in
when they become available during boot:

| Boot phase | COM3 serial | ohcldiag-dev |
|------------|:-----------:|:------------:|
| Very early kernel (entry → memory setup) | ✅ | ❌ |
| Device initialization (VMBus, etc.) | ✅ | ❌ |
| Kernel panic before userspace | ✅ | ❌ missed |
| Init/startup failures | ✅ | ❌ missed |
| After diagnostic service starts | ✅ | ✅ |

COM3 serial output uses direct UART I/O — it's available from the very first
instruction of OpenHCL boot. `ohcldiag-dev` connects over vsock (VMBus), so it's
only available after the kernel boots, VMBus initializes, and the diagnostic
worker starts in userspace.

For most development, `ohcldiag-dev` is sufficient — boot succeeds and you get
logs. COM3 is essential for debugging early boot failures, kernel panics, and
init crashes.

## Enabling COM3 on Hyper-V

COM3 support requires a host OS build that includes the `EnableAdditionalComPorts`
code path. This was added in Windows builds based on `br_release` (build 28000+).
It is **not available** on Windows 11 24H2, 25H2, or Windows Server 2025.

To enable COM3 on a supported build:

```powershell
# Enable additional COM ports (requires reboot or VMMS restart)
reg add "HKLM\Software\Microsoft\Windows NT\CurrentVersion\Virtualization" /v EnableAdditionalComPorts /t REG_DWORD /d 1 /f

# Attach COM3 to a named pipe for a VM
Set-VMComPort -VMName $VmName -Number 3 -Path "\\.\pipe\openhcl-com3"

# Read the serial output
hvc serial -c -p 3 -r $VmName
```

```admonish note
The `flowey` test runner (`install_vmm_tests_deps`) sets this registry key
automatically when running VMM tests. If you run `cargo xflowey` to execute
tests, you'll be prompted to allow the registry change.
```

## Recommended host OS for OpenHCL development

We recommend running a **Windows Insider flight** (Canary channel, build 28000+)
on your development machine. This gives you COM3 support via the registry key
above, plus access to the latest Hyper-V features. This matches the OS used on
the project's self-hosted CI runners.

If you're on Windows 11 24H2/25H2 (builds 26100/26200), COM3 is not available
via the registry key. Use `ohcldiag-dev` for kernel logs instead, or install an
Insider build.

## ARM64 limitation

On Hyper-V, additional serial ports (COM3+) are **not supported on ARM64**. The
Hyper-V serial device for ARM64 does not support ports beyond COM1 and COM2. On
ARM64 hosts, use `ohcldiag-dev` for OpenHCL kernel logs.

This limitation is Hyper-V-specific — when running OpenVMM directly (without
Hyper-V), ARM64 serial output works via PL011 UART.
