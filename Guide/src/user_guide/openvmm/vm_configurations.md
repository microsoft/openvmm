# VM Configurations: Gen1 vs Gen2 Equivalents

If you're familiar with Hyper-V's Gen1 and Gen2 VM concepts, this page maps
those to the equivalent OpenVMM CLI flags.

## Background

Hyper-V defines two VM "generations" that differ in firmware, device model, and
boot mechanism:

| | Gen1 | Gen2 |
|---|---|---|
| Firmware | BIOS (PCAT) | UEFI |
| Boot disk | IDE | SCSI (VMBus storvsp) |
| Guest OS | Legacy and modern (older Windows, DOS, Linux with BIOS support) | Modern (Windows 10+, most Linux) |
| Secure Boot | Not available | Available |

OpenVMM doesn't use the "Gen1/Gen2" terminology — you select the components
directly via CLI flags.

## Gen2-equivalent (UEFI boot) — the common case

Most development and testing uses UEFI boot. This is the default for modern
Windows and Linux guests.

```bash
cargo run -- \
  --uefi \
  --disk memdiff:file:path/to/disk.vhdx \
  -p 4 -m 4GB \
  --gfx
```

Key flags:
- `--uefi` — boot using `mu_msvm` UEFI firmware (implicitly enables Hyper-V
  enlightenments and VMBus, so `--hv` is not needed separately)
- `--disk` — exposes a disk over VMBus (SCSI-equivalent)

## Gen1-equivalent (PCAT BIOS boot)

Use PCAT for operating systems that support BIOS boot. This includes legacy
systems (DOS, older Windows) as well as modern OSes that still support BIOS
boot (most Linux distributions, Windows 10+).

```bash
cargo run -- \
  --pcat \
  --ide memdiff:file:path/to/disk.vhd \
  --gfx
```

Key flags:
- `--pcat` — boot using the Microsoft Hyper-V PCAT BIOS
- `--ide` — expose a disk via emulated IDE controller (the traditional Gen1
  storage path, no `--hv` required)

See the [PCAT BIOS reference](../../reference/devices/firmware/pcat_bios.md) for more
details on PCAT boot, including floppy and optical boot order.

## With OpenHCL (VTL2)

To run with OpenHCL, add `--hv --vtl2` and `--igvm`. You don't need to
separately specify `--uefi` or `--pcat` — the IGVM file contains the OpenHCL
paravisor, and most IGVM builds bundle the
[mu_msvm UEFI firmware](../../reference/devices/firmware/mu_msvm_uefi.md)
for VTL0 guest boot. The build recipe controls whether UEFI is included
(see [IGVM architecture](../../reference/architecture/openhcl/igvm.md)).

Note: `--vtl2` requires `--hv` to be passed explicitly on the command line,
even though other flags like `--uefi` imply it internally.

```bash
cargo run -- \
  --hv --vtl2 \
  --igvm path/to/openhcl.igvm \
  --disk memdiff:file:path/to/disk.vhdx \
  -p 4 -m 4GB
```

```admonish note
OpenVMM does not currently support OpenHCL with PCAT (Gen1-style) boot.
OpenHCL + PCAT is supported on Hyper-V, where the host provides the PCAT
firmware. If you need Gen1-style boot with OpenHCL, use
[Hyper-V](../openhcl/run/hyperv.md) rather than openvmm standalone.
```

See [Running OpenHCL with OpenVMM](../openhcl/run/openvmm.md)
for full setup instructions.

## Quick reference

| Scenario | Flags | Notes |
|----------|-------|-------|
| Modern Windows/Linux guest | `--uefi --disk memdiff:file:disk.vhdx` | Most common |
| With graphical console | add `--gfx` | VNC-based, see [Graphical Console](../../reference/openvmm/graphical_console.md) |
| With networking | add `--nic` | Consomme user-mode NAT |
| With OpenHCL | `--hv --vtl2 --igvm path/to/openhcl.igvm --disk memdiff:file:disk.vhdx` | IGVM carries the paravisor; no `--uefi`/`--pcat` needed |
| Legacy OS (DOS, old Windows) | `--pcat --ide memdiff:file:disk.vhd --gfx` | IDE storage, BIOS boot |
| Linux direct boot (no firmware) | `--kernel vmlinux --initrd initrd` | Skips UEFI/PCAT entirely |
