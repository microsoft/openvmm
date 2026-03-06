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
the kernel panics, no dump is generated. The only diagnostic output is COM3
serial (if enabled), which captures the panic message in real time. If the
diagnostic service was running before the panic, `ohcldiag-dev` may have ring
buffer messages up to that point, but it cannot capture the panic itself since
the service is terminated by the panic.

For debugging kernel-level issues, the best approach is to enable serial output
via COM3 (see below) — it captures output from the very first instruction of
kernel boot.

## Getting OpenHCL kernel logs (COM3 vs ohcldiag-dev)

Two methods exist for capturing OpenHCL kernel (`kmsg`) output:

**COM3 serial** uses direct UART I/O — it streams output from the very first
instruction of OpenHCL boot in real time.

**ohcldiag-dev** connects over vsock to the diagnostic service, which reads
`/dev/kmsg`. Because `/dev/kmsg` preserves the kernel ring buffer, early boot
messages are **replayed** when you connect — you get them even if you connect
late. However, `ohcldiag-dev` only works if the diagnostic service successfully
starts.

| Boot phase | COM3 serial | ohcldiag-dev |
|------------|:-----------:|:------------:|
| Very early kernel (entry → memory setup) | ✅ live | ✅ replayed from ring buffer |
| Device initialization (VMBus, etc.) | ✅ live | ✅ replayed from ring buffer |
| Kernel panic before userspace | ✅ live | ❌ service never starts |
| Boot hang (kernel stuck) | ✅ live | ❌ service never starts |
| After diagnostic service starts | ✅ live | ✅ live |

For most development, `ohcldiag-dev` is sufficient — boot succeeds and you get
logs. COM3 is essential for debugging early boot failures, kernel panics, and
init crashes.

## Enabling COM3 on Hyper-V

COM3 support requires a host OS build that includes the `EnableAdditionalComPorts`
code path. This was added in Windows 11 26H1 (build 28000+, Insider Canary channel).
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

We recommend running a **Windows 11 26H1** Insider flight (Canary channel,
build 28000+) on your development machine, if it is available for your device.
This gives you COM3 support via the registry key above, plus access to the
latest Hyper-V features. This matches the OS used on the project's self-hosted
CI runners.

Note that 26H1 Insider builds may not be available for all hardware — see
[What to know about Windows 11 version 26H1](https://techcommunity.microsoft.com/blog/windows-itpro-blog/what-to-know-about-windows-11-version-26h1/4491941)
for details and the
[Windows Insider Flight Hub](https://learn.microsoft.com/en-us/windows-insider/flight-hub/)
for availability.

If you're on Windows 11 24H2/25H2 (builds 26100/26200), COM3 is not available
via the registry key. Use `ohcldiag-dev` for kernel logs instead.

## ARM64 limitation

On Hyper-V, additional serial ports (COM3+) are **not supported on ARM64**. The
Hyper-V serial device for ARM64 does not support ports beyond COM1 and COM2. On
ARM64 hosts, use `ohcldiag-dev` for OpenHCL kernel logs.

This limitation is Hyper-V-specific — when running OpenVMM directly (without
Hyper-V), ARM64 serial output works via PL011 UART.
