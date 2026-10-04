# Debugging OpenHCL

OpenHCL provides several debugging tools for investigating issues at the
user-mode and kernel level. See [ohcldiag-dev](./diag/ohcldiag_dev.md) for the
diagnostic client and [Tracing](./diag/tracing.md) for serial and event log
tracing.

## ARM64 startup trace diagnostic

This diagnostic branch builds kernel revision
`78489ebc95ec31f426a44062051cf27bd9d9c7d8` with event tracing enabled. The
published 6.18.37.5 ARM64 kernel has `CONFIG_FTRACE` disabled. The initial
captures added instrumentation only. The branch now also validates the
console-load fixes below, without changing default VP entry policies, guest
images, test retries, or VM resource settings.

The Linux/OpenHCL ARM64 heavy boot test enables scheduler switches/wakeups,
native VTL entry/exit, and PL011 console events at kernel boot. All events use
the `mono` clock. Petri consumes `trace_pipe` continuously through the
diagnostics connection into `startup_trace.log`, then freezes and drains the
trace after a 15-second collection window. It does not use serial for
collection. Trace overruns cause an explicit collection error.

The console events identify:

- `record`: port, byte count, atomic/threaded path, and begin/end/aborted stage
  (`0`, `1`, and `2`, respectively).
- `wait`: FIFO/BUSY mask, polling iterations, and elapsed nanoseconds. An event
  is emitted when polling repeats or the initial register read takes 50 us.
- `slow_write`: data-register writes taking at least 50 us.
- `tx_batch`: FIFO capacity, actual status reads, and transmitted bytes for
  each threaded record.
- `mshv_vtl_console_handoff`: request, consumption, and directed-yield result
  for a locally parked VP.

### Console-load fixes under validation

The kernel patch now includes two changes, without changing the default
lower-VTL entry policy or test/VM settings:

- A PL011 printer with more queued records can hand off to a locally parked
  VP after releasing console ownership and all device/SRCU locks. The
  handoff wakes the VP and requests one normally scheduled entry, after
  which the existing reentry policy resumes. No handoff is requested for
  an empty backlog or without a pending local VP.
- Threaded writes reuse FIFO-space credits obtained only when the FIFO is
  empty. Credits are reset per record; newline expansion and console
  takeover points are preserved. Disabled FIFOs, DMA-configured ports,
  and erratum variants retain per-byte status checks. Atomic/panic writes
  retain their existing path.

The production batching helper has executable byte-order, CRLF, backpressure,
one-slot, and record-boundary tests. For an immediately drained 32-byte FIFO,
512 transmitted bytes require 16 status reads instead of 512. Runtime
`tx_batch` counters also include the final BUSY check.

The trace uses 2 MiB per CPU, so account for its memory and execution overhead
when comparing traced and untraced boots. The rebuild uses GCC 13.3 rather
than the release package's GCC 13.2; record the kernel provenance alongside
results. A successful traced boot alone does not validate a proposed fix.

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
The `flowey` test runner (`install_vmm_tests_external_deps`) sets this registry key
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
