This folder contains manifest recipes for building IGVM files. Create the
resource file and run `igvmfilegen manifest` directly.

## SNP Linux-direct profile

`snp-linux-direct.json` is a bring-up profile with these assumptions:

- x64 and one VTL0 SEV-SNP guest that boots Linux directly
- a simple `processor_count`; the default profile uses one virtual processor,
  while `snp-linux-direct-multi-vp.json` uses two
- 160 MiB of contiguous RAM (40,960 4-KiB pages)
- one NUMA node (node 0), containing all CPUs and all RAM
- DT-described CPUs, RAM, UARTs, and optional PCIe bridges, with the fixed
  enlightened x86 chipset
- no shared GPA boundary, normal interrupt injection, and secure AVIC disabled
- base SNP policy `0x30000`; `enable_debug` adds the debug bit to produce the
  current debug-capable policy `0xb0000`
- an initrd and the kernel command line
  `console=ttyS0 earlyprintk=serial earlycon panic=-1`
- SNP C-bit position 51; the value must be bit 32 or higher because the
  startup page tables identity-map the lower 4 GiB

The normal-injection output is a shared artifact: the same binary is intended
to boot on KVM and MSHV. Its `SnpVpContext` uses the SNP initial-VMSA GPA
`0xffff_ffff_f000`. KVM synthesizes its measured VMSA at that GPA, while MSHV
maps and imports the file-provided VMSA there. Both backends use the policy
encoded in the file, but only MSHV submits its SNP ID block.

The `snp-linux-direct-restricted.json` profile encodes restricted interrupt
injection in its IGVM VMSA. It is intended only for MSHV bring-up.

Every SNP Linux-direct image requests the host's device tree through an
unmeasured IGVM parameter area. After launch, the measured bootshim validates
the complete topology and generates DSDT, FADT, MADT, SRAT, XSDT and RSDP.
It adds MCFG and a PCIe SSDT only when bridges are present. Images contain
zero-filled output storage, not prebuilt ACPI tables. No `pcie` or `acpi`
mode selector is needed. `snp-linux-direct-pcie.json` remains a two-VP example
for the PCIe launch command below.

The runtime publishes COM1/COM2 when any serial backend is configured, and
COM3/COM4 only when their individual backend is configured. The shim emits
only the UARTs in that description. OpenHCL still selects a published COM3
as its console. Both consumers use the existing IGVM PIO binding:
`pio-bus/serial@<base>` has `compatible = "ns16550"`, `reg` contains two
big-endian 64-bit values (base and length), and `interrupts` contains one
big-endian 64-bit IRQ. This encoding is retained despite the PIO parent's
one-cell address/size declarations; it is not the native ARM UART binding.

The measured revision-3 handoff supplies the exact CPU count, contiguous RAM
end, and bounded workspace locations. Older experimental images and shims
are incompatible. Rebuild both; there is no static-table fallback.

The image contains a small measured bootshim. Only pages containing the kernel,
initrd, boot metadata, SNP special pages, bootshim, or bootshim parameters are
included as IGVM `PageData`, along with zero-filled permanent ACPI and RSDP
storage. The DT uses private, already-imported parameter pages. The shim
checks DT without allocating before it accepts any omitted pages. After
this preflight, it accepts the
remaining private RAM with `PVALIDATE` and then enters Linux. This avoids
loading and measuring every configured RAM page, but still accepts all RAM
before Linux starts.

The IGVM contains only the BSP VMSA, regardless of processor count. Backends
are responsible for any AP launch state they require. Current KVM constructs
and measures the initial VMSAs itself rather than accepting the IGVM VMSA page.
Those KVM-created VMSAs are not part of the IGVM launch measurement, so the
file's SNP ID block is not valid for KVM. KVM attestation against that ID block
remains unsupported until KVM accepts userspace-provided VMSAs.

To build it manually, create a resources file containing absolute paths:

```json
{
    "resources": {
        "linux_kernel": "/absolute/path/to/vmlinux-or-bzImage",
        "linux_initrd": "/absolute/path/to/initrd",
        "snp_bootshim": "/absolute/path/to/snp_bootshim"
    }
}
```

Build the bootshim first:

```bash
MINIMAL_RT_BUILD=1 cargo build \
  --profile boot-dev \
  --target x86_64-unknown-none \
  -p snp_bootshim
```

Build the host-native generator:

```bash
cargo build -p igvmfilegen
```

Then generate the image:

```bash
repo=/absolute/path/to/openvmm
cargo run -p igvmfilegen -- manifest \
  --manifest "$repo/vm/loader/manifests/snp-linux-direct.json" \
  --resources /absolute/path/to/snp-linux-direct-resources.json \
  --output /absolute/path/to/snp-linux-direct.bin
```

The standard outputs are:

- `snp-linux-direct.bin`
- `snp-linux-direct.bin.map`
- `snp-linux-direct-snp.json`

### Launching the fixed profile on MSHV

The image measures the expected CPU count and RAM bound. Host DT must describe
exactly that count, contiguous APIC IDs starting at zero, BSP zero, and the
same RAM interval on NUMA node zero. DT cannot expand RAM acceptance.
Regenerate the IGVM after changing these measured bounds.

Use one memory node and match both the VP count and memory size to the image:

- `--processors` must equal the manifest's `processor_count`.
- `--memory` must equal `memory_page_count * 4096` bytes.
- Use `--vps-per-socket` equal to the VP count for a single-socket launch with
  the image's contiguous APIC IDs starting at 0. Leave the APIC ID offset at
  its default of 0.
- Use `--memory`, not a multi-node `--numa` configuration. The fixed profile
  assigns every CPU and memory range to NUMA node 0.
- SMT can remain `auto`; it does not require separate NUMA nodes.

For an image generated from `snp-linux-direct-multi-vp.json`:

```bash
openvmm --hypervisor mshv --isolation snp \
  --igvm path/to/snp-linux-direct-multi-vp.bin \
  --igvm-personality linux-direct --hv --no-vmbus \
  --memory 160MB --processors 2 --vps-per-socket 2 --smt auto \
  --com1 console
```

For larger images, change the manifest's `processor_count`, regenerate the
image, and use that count for both `--processors` and `--vps-per-socket`.
Keep COM1 for the profile's `console=ttyS0` kernel command line. If any serial
backend is configured, the host publishes COM1 and COM2. It publishes COM3
and COM4 only when their individual backends are configured. The shim emits
only published UARTs and supports shared IRQ4 for COM1/COM3 and IRQ3 for
COM2/COM4. It does not use console metadata to infer UART presence.

### Host-described PCIe on MSHV

Build `snp_bootshim` and `igvmfilegen` as above, then use the
`snp-linux-direct-pcie.json` manifest. The updated runtime supplies the actual
resolved PCIe layout; no ECAM-base or MMIO-base overrides are needed.

For its two-VP, 160-MiB configuration:

```bash
openvmm --hypervisor mshv --isolation snp --hv --no-vmbus \
  --igvm path/to/snp-linux-direct-pcie.bin \
  --igvm-personality linux-direct \
  --memory 160MB --processors 2 --vps-per-socket 2 \
  --com1 file=path/to/serial.log \
  --pcie-root-complex rc0,segment=0,start_bus=0,end_bus=31,low_mmio=64M,high_mmio=1G,node=0 \
  --pcie-ecam-below-4gb \
  --pcie-root-port rc0:disk0 --pcie-root-port rc0:net0 \
  --virtio-blk file:path/to/disk.raw,pcie_port=disk0 \
  --virtio-net pcie_port=net0:consomme
```

The below-4-GiB option selects a placement class, not a fixed address.
Some direct-boot Linux kernels reject MCFG entries above 4 GiB unless SMBIOS
reports a sufficiently recent BIOS date. Keep this option for those kernels;
the converter supports 64-bit addresses but cannot bypass that guest policy.

This implementation supports at most 255 CPUs, 32 RAM records normalized to
one contiguous interval, four NS16550 UARTs, and eight generic ECAM bridges in
distinct segments, node 0, native x86 MSI/MSI-X, and identity low/high MMIO
windows. A nonempty low window is required; the high window may be omitted.
CXL, IOMMU/remapping, legacy INTx maps, non-identity translations, and preserved
PCI boot configuration are unsupported and rejected. CPU count, APIC IDs and
contiguous RAM size remain image-defined and must still match the launch.

The image reserves 64 KiB for the device tree, 64 KiB for generated ACPI,
a temporary 1-MiB heap, and a 32-KiB bootshim stack. The heap is omitted from
page data and accepted with the RAM gaps before use. ACPI output, RSDP, DT,
the shim, measured handoff pages, and SNP special pages are imported, not
accepted again. No final table points into the DT, heap, or shim.
The bootshim rejects invalid or overlapping windows,
over-capacity input, and incomplete handoffs before entering Linux. The ACPI
arena is ACPI-reclaimable in E820; the pinned RSDP remains reserved.
The shim copies all validated tables to permanent storage before it publishes
the RSDP and Linux zero-page pointer. It never enters Linux with partial ACPI.

Unknown hardware, nonzero NUMA nodes, protectable RAM, and malformed or
duplicate properties, nodes, or identities fail preflight. Known `/chosen`,
`/openhcl`, and partition VMBus metadata is checked but does not add devices
or override the measured Linux boot arguments. The shared PIO binding retains
16-byte `reg` and 8-byte `interrupts` values despite one-cell parent declarations.

Host-selected topology is unmeasured. Attestation policy for those values is
deferred for bring-up; validation does not bind them into the launch identity.
Detailed bootshim failure reporting is also pending: runtime failures use the
standard GHCB general-termination notification rather than continuing with
stale tables.
