# IDE HDD/Optical

The IDE controller emulates a legacy PCI/ISA IDE interface with primary and secondary channels, each supporting up to two devices.

## Overview

IDE is a legacy storage interface carried forward for compatibility with older guest operating systems and boot scenarios. The emulated controller responds to standard ISA I/O port ranges (0x1F0–0x1F7 for primary, 0x170–0x177 for secondary) and a PCI configuration space.

Each drive on a channel is either a hard drive or an optical drive:

- **Hard drives** use ATA commands and call into the `DiskIo` trait directly.
- **Optical drives** use ATAPI commands, which wrap SCSI CDBs. The ATAPI layer delegates to `SimpleScsiDvd`, the same SCSI DVD implementation that StorVSP uses.

The `GuestMedia` enum in `ide_resources` distinguishes the two: `GuestMedia::Disk` for ATA hard drives and `GuestMedia::Dvd` for ATAPI optical drives.

## Limitations

- **No hot-add or hot-remove.** IDE devices are fixed at VM creation.
- **No online disk resize.** IDE has no standardized capacity-change notification mechanism.
- **Two channels, two devices each.** Maximum of four IDE devices per controller.

## Crate

`ide/`

```admonish note title="See also"
- [Storage Pipeline](../../architecture/devices/storage.md) for the full frontend-to-backend architecture and how IDE fits into the pipeline.
- [Storage Pipeline — Virtual optical / DVD](../../architecture/devices/storage.md#virtual-optical--dvd) for the `SimpleScsiDvd` model, eject behavior, and the ATAPI wrapping layer.
```
