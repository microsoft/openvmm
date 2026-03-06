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
  --hv \
  -p 4 -m 4GB \
  --gfx
```

Key flags:
- `--uefi` — boot using `mu_msvm` UEFI firmware
- `--disk` — exposes a disk over VMBus (SCSI-equivalent). Requires `--hv`
- `--hv` — enables Hyper-V enlightenments and VMBus support

## Gen1-equivalent (PCAT BIOS boot)

Use PCAT for operating systems that support BIOS boot. This includes legacy
systems (DOS, older Windows) as well as modern OSes that still support BIOS
boot (most Linux distributions, Windows 10+).

```bash
cargo run -- \
  --pcat \
  --disk memdiff:file:path/to/disk.vhd \
  --gfx
```

Key flags:
- `--pcat` — boot using the Microsoft Hyper-V PCAT BIOS
- IDE storage is used automatically with PCAT (no `--hv` required for basic
  disk access)

See the [PCAT BIOS reference](../../reference/devices/firmware/pcat_bios.md) for more
details on PCAT boot, including floppy and optical boot order.

## With OpenHCL (VTL2)

To run with OpenHCL, add `--vtl2` and `--igvm` to a UEFI configuration. OpenHCL
requires UEFI — it does not work with PCAT.

```bash
cargo run -- \
  --uefi \
  --hv --vtl2 \
  --igvm path/to/openhcl.igvm \
  --disk memdiff:file:path/to/disk.vhdx \
  -p 4 -m 4GB
```

See [Running OpenHCL with OpenVMM](../openhcl/run/openvmm.md)
for full setup instructions.

## Quick reference

| Scenario | Flags | Notes |
|----------|-------|-------|
| Modern Windows/Linux guest | `--uefi --hv --disk memdiff:file:disk.vhdx` | Most common |
| With graphical console | add `--gfx` | VNC-based, see [Graphical Console](../../reference/openvmm/graphical_console.md) |
| With networking | add `--nic` | Consomme user-mode NAT |
| With OpenHCL | add `--vtl2 --igvm path/to/openhcl.igvm` | Requires `--uefi --hv` |
| Legacy OS (DOS, old Windows) | `--pcat --gfx` | IDE storage, BIOS boot |
| Linux direct boot (no firmware) | `--kernel vmlinux --initrd initrd` | Skips UEFI/PCAT entirely |
