# UEFI: mu_msvm

OpenVMM currently uses the `mu_msvm` UEFI firmware package in order to support
booting and running modern EFI-boot capable operating systems.

```admonish question title="What about OVMF, or other firmware packages?"
In the future, it would be useful to also support alternative UEFI firmware
packages, such as [OVMF].

Please reach out of if this is something you may be interested in helping out
with!
```

Two OpenVMM components work in tandem in order to load and run the `mu_msvm`
UEFI firmware:

- Pre-boot: the VMM's UEFI firmware loader:
  1. Reads and validates the firmware's build and interface version information
  2. Writes the `mu_msvm` UEFI firmware package into guest RAM
  3. Writes VM topology information, and `mu_msvm`-specific config data into guest RAM
  4. Initializes register state such that the VM will begin executing from UEFI

- At runtime: the UEFI code within the Guest interfaces with a bespoke
  `firmware_uefi` device in order to implement certain UEFI services, such as
  NVRam variable support, watchdog timers, logging, etc.

## Firmware interface version

Newer `mu_msvm` images contain a machine-readable version record in the combined
PEI/DXE firmware volume. OpenVMM reads this volume at image offset `0x0` on x64
and `0x1000` on AArch64, matching the layout introduced in v26.0.39. It does not
search the separate DXE volume in older AArch64 images. OpenVMM locates the
record's dedicated FFS file by GUID and logs its release version, source commit,
build flags, structure version, and firmware interface version when loading the
image.

OpenVMM accepts the supported interface major version and that major's minimum
minor version or any newer minor version. A present record that is malformed or
has an incompatible interface version prevents the VM from starting. Use
`--uefi force_firmware_version` to log a warning and continue in that case.

When the dedicated FFS file is absent from the volume being checked, OpenVMM
logs a warning and continues loading the image.

## Acquiring a copy of `mu_msvm`

The `cargo xflowey restore-packages` script will automatically pull down a
precompiled copy of the `mu_msvm` UEFI firmware from the [microsoft/mu_msvm]
GitHub repo.

Alternatively, for those that wish to manually download / build `mu_msvm`:
follow the instructions over on the [microsoft/mu_msvm] repo, and ensure the
package is extracted into the `.packages/` directory in the same manner as the
`cargo xflowey restore-packages` script.

[OVMF]: https://github.com/tianocore/tianocore.github.io/wiki/OVMF
[microsoft/mu_msvm]: https://github.com/microsoft/mu_msvm
