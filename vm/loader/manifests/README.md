This folder contains manifest recipes for building IGVM files. Create the
resource file and run `igvmfilegen manifest` directly.

## SNP Linux-direct profile

`snp-linux-direct.json` is a bring-up profile with these assumptions:

- x64 and one VTL0 SEV-SNP guest that boots Linux directly
- a simple `processor_count`; the default profile uses one virtual processor,
  while `snp-linux-direct-multi-vp.json` uses two
- 160 MiB of contiguous RAM (40,960 4-KiB pages)
- one NUMA node (node 0), containing all CPUs and all RAM
- COM1 serial ACPI and the fixed, no-PCIe platform profile
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

The image contains a small measured bootshim. Only pages containing the kernel,
initrd, boot metadata, SNP special pages, bootshim, or bootshim parameters are
included as IGVM `PageData`. After SNP launch, the bootshim accepts the
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

The image embeds its CPU APIC IDs, NUMA affinities, and RAM layout in measured
ACPI tables. OpenVMM launch arguments do not rewrite those tables. Regenerate
the IGVM after changing the manifest or updating the generator's topology
logic; existing images retain their old tables and launch measurements.

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
Keep COM1 for the profile's `console=ttyS0` kernel command line. This profile
does not embed PCIe host bridges, so adding PCIe devices at launch does not
supply the missing ACPI description.
