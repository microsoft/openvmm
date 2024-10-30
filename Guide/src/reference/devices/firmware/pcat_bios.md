# Hyper-V BIOS

OpenVMM currently relies on proprietary Hyper-V "PCAT"[^pcat] BIOS firmware
blobs in order to support booting and running various legacy x86 operating
systems.

```admonish question title="What about SeaBIOS, or other BIOS implementations?"
In the future, it would be great if OpenVMM could support alternative,
open-source x86 BIOS firmwares, such as [SeaBIOS].

Please reach out of if this is something you may be interested in helping out
with!
```

Two OpenVMM components work in tandem in order to load and run the BIOS:

- Pre-boot: the VMM's BIOS firmware loader writes the PCAT BIOS into guest RAM,
  and sets up the initial register state such that the VM will begin executing
  the firmware.

- At runtime: the BIOS code inside the VM communicates with a bespoke
  `firmware_pcat` virtual device, which it uses to fetch information about the
  VM's current topology, and to implement certain BIOS services (such as boot
  logging, efficient spin-looping, etc).

## Acquiring the Hyper-V BIOS Firmware

Unfortunately, due to licensing restrictions, the OpenVMM project is not able to
redistribute copies of the proprietary Hyper-V BIOS firmware blob.

That being said - Windows 11 ships copies of the PCAT BIOS firmware in-box under
`System32` as either `vmfirmwarepcat.dll` or `vmfirmware.dll`. When run on
Windows / WSL2, OpenVMM will automatically scan for these files, and use them if
present.

[^pcat]: Fun fact: the term "PCAT" refers to the venerable [IBM Personal
Computer AT], as a nod to this BIOS's early history as a fairly stock PC/AT
compatible BIOS implementation.

[SeaBIOS]: https://www.seabios.org/SeaBIOS
[IBM Personal Computer AT]: https://en.wikipedia.org/wiki/IBM_Personal_Computer_AT
