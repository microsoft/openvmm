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

## \[Hyper-V] Initramfs unpacking failed: write error, No working init found

A VTL2 out-of-memory panic while unpacking the initramfs can occur on any
configuration, before userspace starts. The Hyper-V-Worker log shows:

```text
Initramfs unpacking failed: write error
check access for rdinit=/underhill-init failed: -2
Kernel panic - not syncing: No working init found
```

`-2` is the missing init binary. `/underhill-init` is absent because the
initramfs never unpacked, so the kernel reports no working init. The kernel
command line in that log carries
`OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=release` for the release manifest and
`OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=debug` for the dev manifest. That token
is how you tell the two images apart.

VTL2 ran out of memory while unpacking the initramfs. Changing the VM
startup RAM does not enlarge VTL2. An 8 GB startup setting sizes the guest
pool, which is separate from VTL2.

For an image built in-tree, VTL2 RAM is `memory_page_count` from the recipe
manifest. Counts are 4 KiB pages. The AArch64 manifests below are one
example.

```bash
cargo xflowey build-igvm aarch64 --release
```

That command reads `vm/loader/manifests/openhcl-aarch64-release.json`:
12288 pages (48 MiB), with command line
`OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=release`.

```bash
cargo xflowey build-igvm aarch64
```

Without `--release`, the same recipe reads
`vm/loader/manifests/openhcl-aarch64-dev.json`: 131072 pages (512 MiB),
with command line `OPENHCL_IGVM_VTL2_GPA_POOL_CONFIG=debug`. That dev
count, 131072 pages, got past this panic on that image. The x64 release
manifest is a separate budget, 17920 pages (70 MiB).

To fix it:

1. Raise `memory_page_count` in the recipe manifest and rebuild the IGVM.
   Keep the count a multiple of 512 pages so the byte size stays
   2 MiB-aligned. `load_openhcl_arm64` rejects an unaligned size. A new
   count takes effect only after you rebuild. 12288 pages remains the
   in-tree AArch64 release budget; use a higher multiple of 512 when
   unpacking fails. See
   [Building OpenHCL](../../dev_guide/getting_started/build_openhcl.md).
2. When the larger image needs more VTL2 memory than the host provides,
   for example the 131072-page (512 MiB) count, also pass
   `-IncreaseVtl2Memory` to `Set-OpenHCLFirmware` on the host:

   ```powershell
   Set-OpenHCLFirmware -Vm $vm -IgvmFile $firmwareFile -IncreaseVtl2Memory
   ```

```admonish warning
`-IncreaseVtl2Memory` complements the manifest change and does not
replace it. The switch selects auto placement and enlarges the host-side
VTL2 window (`Vtl2AddressRangeSize` 1024 MB minus
`Vtl2MmioAddressRangeSize` 512 MB, so 512 MiB of RAM), but it does not
change the image's own `memory_page_count`: a 48 MiB image stays 48 MiB.
```

Create the VM under "Create VM with OpenHCL (but not Trusted Launch)":
[that section](run/hyperv.md#create-vm-with-openhcl-but-not-trusted-launch).

```admonish note title="See also"
[Performance analysis](../../reference/openhcl/diag/ohcldiag_dev/perf.md)
shows a `memory_page_count` edit for trace capture inside VTL2.
```

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
