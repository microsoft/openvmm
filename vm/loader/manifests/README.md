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
- a bzImage kernel with an EFI stub (`CONFIG_EFI_STUB`), and preferably
  `CONFIG_UNACCEPTED_MEMORY`
- lazy memory acceptance; set `"memory_acceptance": "eager"` in the
  `snp_linux_direct` image to accept all RAM in the shim
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

The image contains a small measured bootshim. Only pages containing the
kernel's PE/COFF image, the initrd, the UTF-16 command line, the SMBIOS tables,
the SNP special pages, the bootshim, or the bootshim parameters are included as
IGVM `PageData`, along with zero-filled permanent ACPI and RSDP storage. The DT
uses private, already-imported parameter pages. The shim reads the DT without
allocating before it accepts any omitted pages.

The shim boots Linux through the kernel's own EFI stub. It builds a minimal
UEFI environment: a system table, the boot services that the x86 EFI stub
uses, `LOADED_IMAGE` for the command line, and `LoadFile2` on the
`LINUX_EFI_INITRD_MEDIA_GUID` device path for the initrd. The configuration
tables give the ACPI RSDP, SMBIOS, the SNP CC blob, and an RT properties table
that reports no runtime services. Then the shim calls the kernel's PE/COFF
entry point. The stub decompresses the kernel, builds the E820 map from the
UEFI memory map, calls `ExitBootServices`, and enters the kernel. Linux finds
everything through the UEFI tables, so the image has no zero page.

Linux on SNP reads SMBIOS only after an EFI boot. Its SMBIOS BIOS date lets
Linux accept a PCIe ECAM above 4 GiB, so the image does not need
`--pcie-ecam-below-4gb`.

Memory acceptance is lazy by default. The shim trusts the RAM that the host device tree
describes. It accepts only its ACPI heap, the stack for the EFI stub, a boot
pool for the decompressed kernel, and the omitted RAM below 1 MiB. The UEFI
memory map reports all other omitted RAM, and all host RAM above the measured
RAM end, as `EfiUnacceptedMemory`. When the stub needs more memory, the shim
accepts it on demand. The stub then sets up the Linux unaccepted-memory
bitmap, and Linux accepts the rest when it first uses it. A kernel without
`CONFIG_UNACCEPTED_MEMORY` cannot use unaccepted memory.

With `"memory_acceptance": "eager"`, the shim also accepts all of that RAM
before it calls the stub, and the UEFI memory map has no unaccepted memory.
The image measures this setting.

The shim installs a `#VC` handler before it calls the stub. The handler answers
`CPUID` from the measured SNP CPUID page, and stops the guest for any other
`#VC` reason. The shim's image is EFI runtime-services memory, so its page
tables, its EFI tables, and the handler stay valid until Linux installs its
own.

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
  --pcie-root-port rc0:disk0 --pcie-root-port rc0:net0 \
  --virtio-blk file:path/to/disk.raw,pcie_port=disk0 \
  --virtio-net pcie_port=net0:consomme
```

This implementation supports at most 255 CPUs, 64 RAM ranges that end at or
below 512 GiB, the four fixed COM ports, and eight generic ECAM bridges in
distinct segments. The tables put every bridge on node 0, with native x86
MSI/MSI-X and identity low and high MMIO windows; either window may be
omitted. The bootshim stops the guest when the device tree exceeds these
limits or puts two bridges in one segment. The x86 device tree does not
describe an IOMMU or CXL, so the guest does not see them.

The image reserves 64 KiB for the device tree, 64 KiB for generated ACPI, a
temporary 1-MiB heap, a 256-KiB stack for the EFI stub, and a boot pool of the
bzImage `init_size` plus 8 MiB, between 16 MiB and 512 MiB. The heap, the
stack, and the boot pool are omitted from page data, and the shim accepts them
before use. ACPI output, RSDP, DT, the shim, measured handoff pages, and SNP
special pages are imported, not accepted again. No final table points into the
DT or the heap. The bootshim stops the guest if it cannot parse a PCIe bridge,
or if a PCIe ECAM or MMIO window overlaps RAM or reaches the C-bit. The ACPI
tables and the RSDP are ACPI reclaim memory. The shim copies all tables to
permanent storage before it publishes the RSDP in the configuration table. It
never enters Linux with partial ACPI.

The shim ignores all other device-tree nodes and properties, including the
`/chosen` and VMBus nodes. OpenVMM adds the `/openhcl` node only for
`personality=openhcl`.

Host-selected topology is unmeasured. Attestation policy for those values is
deferred for bring-up; the shim does not bind them into the launch identity.
When the shim stops the guest, it sends a GHCB termination request with
reason-code set 0xF and a reason code from `Failure` in
`vm/loader/snp_bootshim/src/main.rs`.
