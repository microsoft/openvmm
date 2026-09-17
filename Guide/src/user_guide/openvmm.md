# OpenVMM

OpenVMM can be configured to run as a conventional [hosted, or
"type-2"](https://en.wikipedia.org/wiki/Hypervisor#Classification) Virtual
Machine Monitor (VMM).

At the moment, OpenVMM can be built and run on the following host platforms:

| Host OS | Architecture  | Virtualization API                |
| ------- | ------------- | --------------------------------- |
| Windows | x64 / Aarch64 | WHP (Windows Hypervisor Platform) |
| Linux   | x64 / Aarch64 | KVM                               |
|         | x64 / Aarch64 | MSHV (Microsoft Hypervisor)       |
| macOS   | Aarch64       | Hypervisor.framework              |

When compiled, OpenVMM consists of a single standalone `openvmm` / `openvmm.exe`
executable.[^dlls]

```admonish note
As you explore the OpenVMM repo, you may find references to the term **HvLite**.

HvLite was the former codename for OpenVMM, so whenever you see the term
"HvLite", you can treat it as synonymous to "OpenVMM".

We are actively migrating existing code and docs away from using the term
"HvLite".
```

## Notable Features

This *non-exhaustive* list provides a broad overview of some notable features,
devices, and scenarios OpenVMM currently supports.

- Boot modes
  - UEFI - via [`microsoft/mu_msvm`](https://github.com/microsoft/mu_msvm) firmware
  - BIOS - via the [Hyper-V PCAT BIOS](../reference/devices/firmware/pcat_bios.md) firmware
  - Linux Direct Boot
- Devices
  - Paravirtualized
    - [Virtio](https://wiki.osdev.org/Virtio)
      - virtio-fs
      - virtio-9p
      - virtio-net
      - virtio-pmem
    - [VMBus](https://docs.kernel.org/virt/hyperv/vmbus.html)
      - storvsp
      - netvsp
      - serial
      - framebuffer
      - keyboard / mouse
      - vpci
  - Direct Assigned (experimental, WHP only)
  - Emulated
    - vTPM
    - NVMe
    - Serial UARTs (both 16550, and PL011)
    - Legacy x86
      - i440BX + PIIX4 chipset (PS/2 kbd/mouse, RTC, PIT, etc)
      - IDE HDD/Optical, Floppy
      - PCI
      - VGA graphics (experimental)
- Device backends
  - Graphics / Mouse / Keyboard (VNC)
  - Serial (term, socket, tcp)
  - Storage (raw img, VHD/VHDx, Linux blockdev, HTTP)
  - Networking (various)
- Management interfaces (evolving)
  - CLI
  - Interactive console
  - gRPC
  - ttrpc

For more information on any / all of these features, see their corresponding
pages under the **Reference** section of the OpenVMM Guide.

```admonish note title="Management interface compatibility"
OpenVMM's management interfaces continue to evolve and may change between
releases. Consult the documentation for your OpenVMM version, especially when
upgrading.
```

[^dlls]: though, depending on the platform and compiled-in feature-set, some
    additional DLLs and/or system libraries may need to be installed.
