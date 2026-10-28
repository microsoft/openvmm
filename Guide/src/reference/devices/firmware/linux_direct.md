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

The x86-64 microVM profile uses the Linux zero-page protocol with an
uncompressed ELF kernel, but selects an ACPI-free platform mode. OpenVMM places
an Intel MP 1.4 floating pointer at `0x0`, its configuration table at `0x400`,
the bootstrap GDT at `0x1000`, `boot_params` at `0x2000`, and the command line
in the 64-KiB region starting at `0x20000`. The MP table describes contiguous
APIC IDs, the ISA bus, the IOAPIC, and legacy IRQ routing. No RSDP, ACPI table,
SMBIOS anchor, or SMBIOS structure table is imported.

On AArch64, pass the uncompressed `Image` file (not `Image.gz`).

## x86_64 Boot Flow

On x86_64, OpenVMM follows the standard Linux boot protocol:

1. The kernel image is loaded at the conventional 1 MB address.
2. An initrd (if provided) is placed after the kernel.
3. A **zero page** is constructed containing the memory map, command line
   pointer, and initrd location.
4. For the standard profile, ACPI tables (MADT, FADT, DSDT, SRAT, etc.) are built by OpenVMM's ACPI
   builder and placed in low memory just above the boot metadata. The RSDP is
   placed at the fixed `0xE0000` and the kernel discovers it through its legacy
   firmware scan of `[0xE0000, 0x100000)`; the RSDP's XSDT pointer references
   the tables below it.
5. For the standard profile, **SMBIOS (DMI) tables** are synthesized. Because there is no firmware to
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
   Guests can then read `/sys/class/dmi/id/*`.
6. A GDT and initial page tables are set up.
7. The BSP register state is configured and execution begins.

The DSDT includes whatever x86 chipset devices are configured (serial ports,
IOAPIC, PCI bus, VMBus, virtio-mmio, RTC, etc.).

For `--machine microvm`, steps 4 and 5 are replaced by the MP-table layout
described above. The zero page leaves `acpi_rsdp_addr` zero, reserves the ISA
hole and the live shared-status page in e820, and leaves the fixed 3-to-4-GiB
MMIO aperture out of RAM. The loader enters the ELF kernel in long mode with
`RSI=0x2000`, `CR3=0x4000`, and paging enabled.

The microVM virtio devices may reach only the RAM ranges of this e820 map with
DMA, which after a restore include the RAM that the restore adds. A device
refuses a queue whose rings, or a descriptor whose buffer, lies even partly
outside them, such as in the shared-status page, in the ISA hole, or across the
aperture: it reads and writes none of it, and its transport sets
`DEVICE_NEEDS_RESET` in the device status, with a configuration-change
notification if the driver has set `DRIVER_OK`. A refused ring fails
`DRIVER_OK`. Resetting the device clears the mark.

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

## SEV-SNP IGVM Boot

OpenVMM can also boot an SEV-SNP Linux guest from an IGVM image that
`igvmfilegen` builds from the `snp-linux-direct*.json` manifests in
`vm/loader/manifests/`. Select the image with `--isolation snp` and
`--igvm firmware=<FILE>,personality=linux-direct`. The image contains the
kernel, the initrd, the boot metadata, and a small measured bootshim,
`snp_bootshim`.

The image does not contain ACPI tables. At launch, OpenVMM writes its
[device tree](../../architecture/openvmm/device-tree.md) into an unmeasured
IGVM parameter area. The bootshim reads the CPUs, RAM, PCIe host bridges, and
COM ports from that tree, accepts the guest RAM, and then builds the DSDT,
FADT, MADT, SRAT, XSDT, and RSDP. It adds an MCFG and a PCIe SSDT when the
device tree describes host bridges.

### Launch Requirements

One image boots with any supported CPU count, RAM size, and NUMA layout:

- Give the guest at least the RAM that the image measures, which is
  `memory_page_count` pages in the manifest. The shim trusts and accepts any
  RAM that the device tree describes above that.
- Use up to 255 processors and 64 RAM ranges, with all RAM below 512 GiB.
  The bootshim stops the guest when the configuration is larger.

### PCIe

The image does not fix the ECAM or BAR-window addresses. Use the usual
`--pcie-root-complex`, `--pcie-root-port`, and PCIe virtio device options.
Some direct-boot kernels reject an ECAM above 4 GiB unless SMBIOS reports a
recent BIOS date. For these kernels, also use `--pcie-ecam-below-4gb`.

The bootshim supports up to eight generic ECAM host bridges in distinct
segments. It puts every bridge on NUMA node 0, with identity MMIO windows and
native x86 MSI/MSI-X. The guest does not see an IOMMU or CXL.

See `vm/loader/manifests/README.md` for the build and launch recipe.

## SMBIOS / DMI Identity

For standard-profile direct boot on both architectures, synthesized SMBIOS
tables expose a default identity:

| sysfs file | Default value |
|------------|---------------|
| `sys_vendor` | `OpenVMM` |
| `product_name` | `OpenVMM Virtual Machine` |
| `product_uuid` | Not exposed unless the VM's BIOS GUID is set |

The `product_uuid` is sourced from the same VM BIOS GUID used by the UEFI boot
path, so a guest reports the same UUID whether booted via UEFI or direct boot.
The SMBIOS UUID defaults to the all-zero GUID, which Linux treats as not
present and therefore does not expose as `product_uuid`. Pass
`--smbios type=1,uuid=<GUID>` (or `uuid=random`) to set it.

The microVM MP-table mode exposes no SMBIOS data and rejects SMBIOS overrides.

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
