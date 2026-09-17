# Firmware and Boot Modes

OpenVMM supports several ways to boot a guest VM, each with different
firmware requirements and guest OS compatibility:

| Boot mode | Architecture | Firmware | Use case |
|-----------|-------------|----------|----------|
| **UEFI** | x86_64, AArch64 | [mu_msvm](./mu_msvm_uefi.md) | Windows, modern Linux, full UEFI environment |
| **PCAT BIOS** | x86_64 | [Hyper-V PCAT BIOS](./pcat_bios.md) | Legacy OS, Gen1-style boot |
| **Linux Direct** | x86_64, AArch64 | None (VMM is the bootloader) | [Fast Linux boot](./linux_direct.md), development, testing |
| **IGVM** | x86_64, AArch64 | Packaged in IGVM file | OpenHCL paravisor, confidential VMs |

The boot mode is selected by which `--kernel`, `--uefi`, `--pcat`, or
`--igvm` flag is passed on the command line (or the equivalent ttrpc
configuration).

```admonish note
Not all boot modes are available on all architectures — see the table
above for supported combinations.
```
