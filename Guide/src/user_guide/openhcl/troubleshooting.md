# OpenHCL Troubleshooting

This page includes a miscellaneous collection of troubleshooting tips for common
issues you may encounter when running OpenHCL.

If you are still running into issues, consider filing an issue on the OpenVMM
GitHub Issue tracker.

## \[Hyper-V] VTL2/VTL0 failed to start

VTL2/VTL0 fails to boot is when either VTL2 or VTL0 has crashed. When the crash happens, they will emit an event to the Hyper-V worker channel.

First, check `Hyper-V worker  events` at `Applications and Services Logs -> Microsoft -> Windows -> Hyper-V-Worker-Admin`

Alternatively, some queries you can use to get Hyper-V-Worker logs:

- Display the `{n}` most recent events -  `wevtutil qe Microsoft-Windows-Hyper-V-Worker-Admin /c:{n} /rd:true /f:text`
- Export events to file - `wevtutil epl Microsoft-Windows-Hyper-V-Worker-Admin C:\vtl2_0_crash.evtx`

## Checking OpenHCL logging output

OpenHCL logging output can be useful for debugging issues with startup or runtime behavior.

See [OpenHCL Tracing](../../reference/openhcl/diag/tracing.md) for more details about how to enable OpenHCL logging.

## DeviceTree errors or warnings in the VTL2 kernel log

1. Retrieve the DeviceTree blob from OpenHCL:

```powershell
uhdiag-dev.exe linux-uhvm00 file --file-path "/sys/firmware/fdt" > uh.dtb
```

1. Install the DeviceTree compiler and convert the blob to the textual representation:

```sh
sudo apt-get install dtc
dtc -I dtb -o uh.dts uh.dtb
```

Check on the errors and warnings and should any have been produced, fix them in the
DeviceTree generation code. If that doesn't resolve the issues, inspect the DT parsing
code in the Linux kernel.
