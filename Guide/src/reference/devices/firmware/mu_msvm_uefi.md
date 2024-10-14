# UEFI: mu_msvm

OpenVMM currently uses the `mu_msvm` UEFI firmware package in order to support
booting and running modern EFI-boot capable operating systems.

> In the future, it would be useful to also support alternative UEFI firmware
> packages, such as [OVMF].
>
> Please reach out of if this is something you may be interested in helping out
> with!

Two OpenVMM components work in tandem in order to load and run the `mu_msvm`
UEFI firmware:

- Pre-boot: the VMM's UEFI firmware loader does  3 things:
  1. Writes the `mu_msvm` UEFI firmware package into guest RAM
  2. Writes VM topology information, and `mu_msvm`-specific config data into guest RAM
  3. Initializes register state such that the VM will begin executing from UEFI

- At runtime: the UEFI code within the Guest interfaces with a bespoke
  `firmware_uefi` device in order to implement certain UEFI services, such as
  NVRam variable support, watchdog timers, logging, etc.

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
