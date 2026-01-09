# OpenHCL Tracing

## Using `ohcldiag-dev`

[`ohcldiag-dev`](./ohcldiag_dev.md) offers several methods for collecting
different sorts of traces from OpenHCL.

We suggest starting here, before exploring some of the other options presented
on this page.

## Enabling serial logging for OpenHCL

The OpenHCL startup binary accepts a commmand line argument `OPENVMM_LOG` that can be used to enable global or discrete tracing levels.
These logs are written by default to `COM3` and OpenVMM can be configured to redirect them to a file or console output.

For examples on how to enable `OPENVMM_LOG`, see [OpenVMM Logging](../../openvmm/logging.md).

## \[Hyper-V\] Saving traces to the Windows event log

The OpenHCL traces can be saved to the Windows event log on the host. That is
not meant to be a production scenario due to resource consumption concerns. By
default, only ETW is emitted.

Setting `PersistentGel` under the virt. key (`HKLM:SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization`)
to `1` (`REG_DWORD`) makes the messages being stored to the host event log, too, to make getting traces
easier in the development scenarios. The traces will be stored under the Hyper-V Worker Operational log.

Here is a Powershell one-liner to enable that developer aid:

```pwsh
New-ItemProperty "HKLM:SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization" -Name "PersistentGel" -Value 1 -Type Dword -Force
```

*Note*: the key has to be set prior to starting the VM to save the logging messages sent by VTL2 running
inside that VM to the Windows event log.

To retrieve the events with Powershell, start with this one-liner and tweak it to your needs:

```pwsh
Get-WinEvent -FilterHashtable @{ LogName='Microsoft-Windows-Hyper-V-Worker-Operational'; ProviderName='Microsoft-Windows-Hyper-V-Chipset' }
```

## (Advanced) Enable Linux Kernel Tracing

Sometimes it can be useful to extract additional information from the kernel
during runtime. By default the config OpenHCL uses does not support tracing;
as such you will need to build a custom kernel with tracing support. First, see
the [Kernel Development](../../../dev_guide/getting_started/build_ohcl_kernel.md)
section of the docs to find the repo. To set up a tracing enabled kernel:

1. Find `CONFIG_FTRACE` in Microsoft/hcl-dev.config and change it from
   `CONFIG_FTRACE is not set` to `CONFIG_FTRACE=y`.
2. Build the kernel using the Microsoft/build-hcl-kernel.sh script.
3. In the loader json you intend to use, change the `kernel_path` entry to point
   to your newly built vmlinux. This can usually be found at
   linux-dom0-hyperv/out/vmlinux.
4. Build OpenHCL using `cargo xflowey build-igvm --custom-kernel path/to/vmlinux`.
5. When launching your OpenHCL vm, be sure to Set-VmFirmwareParameters
   correctly. The following is an example that enables tracing hyper-v linux
   components such as vmbus: `tp_printk=1 trace_event=hyperv`
   * `tp_printk=1` tells the kernel to print traces to the kernel log.
   * `trace_events=<module>` tells the kernel which module traces to print.
