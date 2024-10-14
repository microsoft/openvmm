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

- Pre-boot: the UEFI firmware loader writes the `mu_msvm` firmware into guest
  RAM, and sets up the initial register state such that the VM will begin
  executing the firmware.

- At runtime: UEFI code inside the VM communicates with a bespoke
  `firmware_uefi` virtual device, which it uses to fetch information about the
  VM's current topology, and to implement certain UEFI services (notably: NVRam
  variables).

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
