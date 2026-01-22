# OpenVMM

OpenVMM can be configured to run as a conventional [hosted, or
"type-2"](https://en.wikipedia.org/wiki/Hypervisor#Classification) Virtual
Machine Monitor (VMM).

At the moment, OpenVMM can be built and run on the following host platforms:

| Host OS | Architecture  | Virtualization API                |
| ------- | ------------- | --------------------------------- |
| Windows | x64 / Aarch64 | WHP (Windows Hypervisor Platform) |
| Linux   | x64           | KVM                               |
|         | x64           | MSHV (Microsoft Hypervisor)       |
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
      - virtio-serial
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
- Management APIs (unstable)
  - CLI
  - Interactive console
  - gRPC
  - ttrpc

For more information on any / all of these features, see their corresponding
pages under the **Reference** section of the OpenVMM Guide.

...though, as you may be able to tell by looking at the sidebar, that section of
the Guide is currently under construction, and not all items have corresponding
pages at this time.

* * *

Before heading on to [Running OpenVMM](./openvmm/run.md), please take a moment
to read and understand the following important disclaimer:

```admonish warning title="DISCLAIMER"
In recent years, development efforts in the OpenVMM project have primarily
focused on [OpenHCL](./openhcl.md) (AKA: OpenVMM as a paravisor).

As a result, not a lot of "polish" has gone into making the experience of
running OpenVMM in traditional host contexts particularly "pleasant".
This lack of polish manifests in several ways, including but not limited to:

- Unorganized and minimally documented management interfaces (e.g: CLI, ttrpc/grpc)
- Unoptimized device backend performance (e.g: for storage, networking, graphics)
- Unexpectedly missing device features (e.g: legacy IDE drive, PS/2 mouse features)
- **No API or feature-set stability guarantees whatsoever.**

At this time, OpenVMM _on the host_ is not yet ready to run end-user
workloads, and should should be treated more akin to a development platform
for implementing new OpenVMM features, rather than a ready-to-deploy
application.
```

[^dlls]: though, depending on the platform and compiled-in feature-set, some
    additional DLLs and/or system libraries may need to be installed.
