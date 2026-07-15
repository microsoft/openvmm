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
| x86_64 | Yes | Uncompressed ELF (`vmlinux`) or compressed `bzImage` | Linux boot protocol (zero page) |
| AArch64 | Yes | ARM64 `Image` (flat binary) | ARM64 Image boot (device tree or ACPI) |

On x86_64, both uncompressed `vmlinux` ELF images and compressed `bzImage`
(vmlinuz) files are supported. When a bzImage is detected, the loader places
the protected-mode code directly into guest memory and relies on the kernel's
built-in decompressor to run at boot time. All standard bzImage compression
formats are supported since decompression is handled by the kernel itself.

On AArch64, pass the uncompressed `Image` file (not `Image.gz`).

## x86_64 Boot Flow

On x86_64, OpenVMM follows the standard Linux boot protocol:

1. The kernel image is loaded at the conventional 1 MB address.
2. An initrd (if provided) is placed after the kernel.
3. A **zero page** is constructed containing the memory map, command line
   pointer, and initrd location.
4. ACPI tables (MADT, FADT, DSDT, SRAT, etc.) are built by OpenVMM's ACPI
   builder and placed in low memory just above the boot metadata. The RSDP is
   placed at the fixed `0xE0000` and the kernel discovers it through its legacy
   firmware scan of `[0xE0000, 0x100000)`; the RSDP's XSDT pointer references
   the tables below it.
5. **SMBIOS (DMI) tables** are synthesized. Because there is no firmware to
   build them, OpenVMM constructs a SMBIOS 3.0 (64-bit) entry point (`_SM3_`)
   plus a minimal structure table (Type 0 BIOS, Type 1 System, Type 127
   end-of-table) itself. ("SMBIOS 3.0 (64-bit) Entry Point" is the spec's name
   for the entry-point format; the tables themselves conform to SMBIOS 3.1, as
   reported in the entry point's version fields.) Only the 24-byte `_SM3_`
   anchor is pinned in the
   F-segment at `0xF0000`, where the kernel finds it via its non-EFI DMI scan
   of `[0xF0000, 0x100000)`; the anchor's 64-bit pointer targets the structure
   table, which lives in its own reserved region in low memory just above the
   ACPI tables (so it can grow well past the 64 KiB F-segment). Both regions
   are reserved in the e820 map so they are not overwritten before the scan.
   Guests can then read `/sys/class/dmi/id/*`. There is no configuration
   surface yet, so every direct-boot VM reports a fixed default OpenVMM
   identity.
6. A GDT and initial page tables are set up.
7. The BSP register state is configured and execution begins.

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
   redistributors or GICv2 CPU interfaces, GICv3 ITS or v2m MSI frame), GTDT
   (virtual timer), DSDT (VMBus, serial UARTs), and optionally MCFG/SSDT for
   PCIe and IORT for PCIe interrupt routing via the ITS.

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
