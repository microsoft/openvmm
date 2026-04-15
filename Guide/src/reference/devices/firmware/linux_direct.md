# Linux Direct Boot

Linux direct boot allows OpenVMM to load a Linux kernel directly into guest
memory without UEFI or BIOS firmware. The VMM itself acts as the bootloader:
it parses the kernel image, places the initrd, constructs the necessary boot
metadata, sets the initial register state, and starts execution at the kernel
entry point.

This is the fastest path from "run" to a Linux userspace prompt, and is
useful for lightweight testing and development scenarios.

## Architecture Support

| Architecture | Supported | Kernel format | Boot protocol |
|-------------|-----------|---------------|---------------|
| x86_64 | Yes | Uncompressed ELF (`vmlinux`) | Linux boot protocol (zero page) |
| AArch64 | Yes | ARM64 `Image` (flat binary) | ARM64 Image boot (device tree or ACPI) |

Compressed kernels (bzImage, gzip, etc.) are not supported. On x86_64,
pass the uncompressed `vmlinux` ELF. On AArch64, pass the uncompressed
`Image` file (not `Image.gz`).

## x86_64 Boot Flow

On x86_64, OpenVMM follows the standard Linux boot protocol:

1. The kernel image is loaded at the conventional 1 MB address.
2. An initrd (if provided) is placed after the kernel.
3. A **zero page** is constructed containing the memory map, command line
   pointer, and initrd location.
4. ACPI tables (MADT, FADT, DSDT, SRAT, etc.) are built by OpenVMM's ACPI
   builder and written at `0xE0000`, where the kernel finds the RSDP via
   its standard firmware scan.
5. A GDT and initial page tables are set up.
6. The BSP register state is configured and execution begins.

The DSDT includes whatever x86 chipset devices are configured (serial ports,
IOAPIC, PCI bus, VMBus, virtio-mmio, RTC, etc.).

## AArch64 Boot Flow

On AArch64, OpenVMM supports two modes for presenting hardware descriptions to
the kernel, selected by the `--device-tree` CLI flag:

### ACPI Mode (default)

This is the default. The kernel discovers devices through ACPI tables, just as
it would on a server with UEFI firmware.

Since the ARM64 kernel's ACPI code path requires entering through the EFI stub,
OpenVMM synthesizes a minimal set of EFI structures in guest memory:

1. **EFI System Table** — points to a configuration table with the ACPI RSDP
   and an RT Properties entry that advertises no runtime services.
2. **EFI Memory Map** — describes the EFI metadata region, ACPI tables, and
   conventional RAM.
3. **ACPI Tables** — FADT (with `HW_REDUCED_ACPI`), MADT (GIC distributor, GICv3
   redistributors or GICv2 CPU interfaces, optional v2m MSI frame), GTDT
   (virtual timer), DSDT (VMBus, serial UARTs), and optionally MCFG/SSDT for
   PCIe.

A **stub device tree** is then built. Unlike a full device tree, it contains
no hardware nodes — no CPUs, GIC, timer, or devices. Its only purpose is a
`/chosen` node with `linux,uefi-system-table` and `linux,uefi-mmap-*`
properties that point the kernel's EFI stub to the synthesized EFI structures.
From there, the kernel follows its standard ACPI discovery path.

```admonish tip title="When to use ACPI mode"
ACPI mode is the default and is recommended when running with the
Hyper-V hypervisor (`--hv`). Device tree mode also supports VMBus
(with recent kernels and hypervisor versions), but ACPI mode provides
broader compatibility.
```

### Device Tree Mode (`--device-tree`)

In this mode, a full device tree is built describing all hardware
directly — CPUs, interrupt controller, timers, serial ports, VMBus,
PCIe bridges, and memory regions. The kernel discovers everything
from the DT; no EFI structures or ACPI tables are involved.

```admonish note
Device tree mode is not supported on x86_64. Passing `--device-tree` on x86
will result in an error.
```

## CLI Usage

```bash
# x86_64 Linux direct boot
openvmm --kernel path/to/vmlinux --initrd path/to/initrd \
    --cmdline "console=ttyS0"

# AArch64 ACPI mode (default)
openvmm --kernel path/to/Image --initrd path/to/initrd \
    --cmdline "console=ttyAMA0 earlycon"

# AArch64 device tree mode
openvmm --kernel path/to/Image --initrd path/to/initrd \
    --cmdline "console=ttyAMA0 earlycon" --device-tree
```
