This folder contains manifest recipes for building IGVM files. Create the
resource file and run `igvmfilegen manifest` directly.

## SNP Linux-direct profile

Flowey builds `snp-linux-direct-pipette.json` for the `linux_direct_x64[snp]`
VMM test configuration. It injects the statically linked Linux pipette into the shared test
initrd from `openvmm-deps`, then runs it as PID 1 with `rdinit=/pipette`. The
test also uses the SNP guest kernel from `openvmm-deps` and the in-tree
`snp_bootshim`. The manual `snp-linux-direct.json` bring-up profile instead
starts the initrd shell. These profiles have these assumptions:

- x64 and one VTL0 SEV-SNP guest that boots Linux directly
- at least 160 MiB of RAM (40,960 4-KiB pages), which the image measures
- DT-described CPUs, RAM, NUMA nodes, UARTs, and optional PCIe bridges, with
  the fixed enlightened x86 chipset
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
unmeasured IGVM parameter area. After launch, the measured bootshim reads the
CPUs, RAM, PCIe host bridges, and COM ports from the tree and generates DSDT,
FADT, MADT, SRAT, XSDT and RSDP. It adds MCFG and a PCIe SSDT only when
bridges are present. Images contain zero-filled output storage, not prebuilt
ACPI tables. One image therefore boots with any supported CPU count, RAM
size, NUMA layout, and PCIe configuration.

The runtime describes COM1/COM2 when any serial backend is configured, and
COM3/COM4 only when their individual backend is configured. The shim emits
only the UARTs in that description. OpenHCL still selects a described COM3
as its console. Both consumers use the existing IGVM PIO binding, in which
the `reg` property of `pio-bus/serial@<base>` starts with a big-endian 64-bit
base. The shim finds each COM port by that base and uses the fixed COM I/O
port and IRQ.

The measured handoff supplies the end of the measured RAM and bounded
workspace locations. Older experimental images and shims are incompatible.
Rebuild both; there is no static-table fallback.

The image contains a small measured bootshim. Only pages containing the kernel,
initrd, boot metadata, SNP special pages, bootshim, or bootshim parameters are
included as IGVM `PageData`, along with zero-filled permanent ACPI and RSDP
storage. The DT uses private, already-imported parameter pages. The shim
reads the DT without allocating before it accepts any omitted pages. Then it
accepts the remaining private RAM with `PVALIDATE` and enters Linux. This avoids
loading and measuring every configured RAM page, but still accepts all RAM
before Linux starts.

The shim trusts the RAM that the host device tree describes. Below the
measured RAM end, it accepts only the gaps that the measured parameter page
lists, so it never accepts a measured page again. It accepts all host RAM
above the measured RAM end and adds that RAM to the Linux E820 map.

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

### Launching on MSHV

The guest needs at least `memory_page_count * 4096` bytes of RAM. Choose the
CPU count, larger RAM sizes, and NUMA layout at launch:

```bash
openvmm --hypervisor mshv --isolation snp \
  --igvm firmware=path/to/snp-linux-direct.bin,personality=linux-direct \
  --hv --no-vmbus \
  --memory 2GB --processors 4 \
  --com1 console
```

Keep COM1 for the profile's `console=ttyS0` kernel command line. If any serial
backend is configured, the host describes COM1 and COM2. It describes COM3
and COM4 only when their individual backends are configured. The shim emits
only described UARTs and supports shared IRQ4 for COM1/COM3 and IRQ3 for
COM2/COM4. It does not use console metadata to infer UART presence.

### Host-described PCIe on MSHV

Use the same image. The runtime supplies the resolved PCIe layout; no
ECAM-base or MMIO-base overrides are needed. For example:

```bash
openvmm --hypervisor mshv --isolation snp --hv --no-vmbus \
  --igvm firmware=path/to/snp-linux-direct.bin,personality=linux-direct \
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

This implementation supports at most 255 CPUs, 64 RAM ranges that end at or
below 512 GiB, the four fixed COM ports, and eight generic ECAM bridges in
distinct segments. The tables put every bridge on node 0, with native x86
MSI/MSI-X and identity low and high MMIO windows; either window may be
omitted. The bootshim stops the guest when the device tree exceeds these
limits or puts two bridges in one segment. The x86 device tree does not
describe an IOMMU or CXL, so the guest does not see them.

The image reserves 64 KiB for the device tree, 64 KiB for generated ACPI,
a temporary 1-MiB heap, and a 32-KiB bootshim stack. The heap is omitted from
page data and accepted with the RAM gaps before use. ACPI output, RSDP, DT,
the shim, measured handoff pages, and SNP special pages are imported, not
accepted again. No final table points into the DT, heap, or shim.
The bootshim stops the guest if it cannot parse a PCIe bridge, or if a PCIe
ECAM or MMIO window overlaps RAM or reaches the C-bit. The ACPI arena is
ACPI-reclaimable in E820; the pinned RSDP remains reserved. The shim copies
all tables to permanent storage before it publishes the RSDP and Linux
zero-page pointer. It never enters Linux with partial ACPI.

The shim ignores all other device-tree nodes and properties, including the
`/chosen` and VMBus nodes. OpenVMM adds the `/openhcl` node only for
`personality=openhcl`.

Host-selected topology is unmeasured. Attestation policy for those values is
deferred for bring-up; the shim does not bind them into the launch identity.
When the shim stops the guest, it sends a GHCB termination request with
reason-code set 0xF and a reason code from `Failure` in
`vm/loader/snp_bootshim/src/main.rs`.
