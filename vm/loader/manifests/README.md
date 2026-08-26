This folder contains manifest recipes for building IGVM files. The build system
normally creates the resource file that supplies each recipe's binary inputs.

## SNP Linux-direct profile

`snp-linux-direct.json` is a bring-up profile with these assumptions:

- x64 and one VTL0 SEV-SNP guest that boots Linux directly
- one virtual processor; `snp-linux-direct-multi-vp.json` uses two
- 160 MiB of contiguous RAM (40,960 4-KiB pages)
- COM1-only serial ACPI, with no VMBus, PCIe, disks, IOMMU, or PSP
- no shared GPA boundary, normal interrupt injection, and secure AVIC disabled
- base SNP policy `0x30000`; `enable_debug` adds the debug bit to produce the
  current debug-capable policy `0xb0000`
- an initrd and the kernel command line
  `console=ttyS0 earlyprintk=serial earlycon panic=-1`
- SNP C-bit position 51, which is a test-host assumption rather than a portable
  SNP property; the generator requires bit 32 or higher because the startup
  page tables identity-map the lower 4 GiB

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
        "linux_initrd": "/absolute/path/to/initrd"
    }
}
```

Then run:

```bash
cargo run -p igvmfilegen -- manifest \
  --manifest /absolute/path/to/openvmm/vm/loader/manifests/snp-linux-direct.json \
  --resources /absolute/path/to/snp-linux-direct-resources.json \
  --output /absolute/path/to/snp-linux-direct.bin
```

The standard outputs are:

- `snp-linux-direct.bin`
- `snp-linux-direct.bin.map`
- `snp-linux-direct-snp.json`
