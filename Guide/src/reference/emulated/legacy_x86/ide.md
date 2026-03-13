# IDE HDD/Optical

The IDE controller emulates the storage portion of an Intel PIIX4
(82371AB) PCI-to-ISA bridge. It provides two IDE channels (primary and
secondary), each supporting up to two devices — four total. Devices are
either ATA hard drives or ATAPI optical drives.

The controller connects to the storage stack through
[`Disk`](https://openvmm.dev/rustdoc/linux/disk_backend/struct.Disk.html)
for hard drives and
[`AsyncScsiDisk`](https://openvmm.dev/rustdoc/linux/scsi_core/trait.AsyncScsiDisk.html)
(via
[`SimpleScsiDvd`](https://openvmm.dev/rustdoc/linux/scsidisk/scsidvd/struct.SimpleScsiDvd.html))
for optical drives. Interrupts use IRQ 14 (primary) and IRQ 15
(secondary). The emulator implements a subset of
[ATA/ATAPI-6](https://www.t13.org/standards-published) (48-bit LBA)
and the ATAPI packet interface from ATA/ATAPI-4, with a PCI config
space layout based on the Intel PIIX4 (82371AB) datasheet (PCI
vendor/device ID `8086:7111`).

## I/O port layout

Command block registers (per channel):

| Port (pri / sec) | Register | Access | Purpose |
|-------------------|----------|--------|---------|
| 0x1F0 / 0x170 | Data | R/W | 16-bit PIO data transfer |
| 0x1F1 / 0x171 | Error (R) / Features (W) | R/W | Error status / command parameters |
| 0x1F2 / 0x172 | Sector count | R/W | Transfer size in sectors |
| 0x1F3–0x1F5 / 0x173–0x175 | LBA low / mid / high | R/W | LBA address (28-bit or 48-bit with HOB) |
| 0x1F6 / 0x176 | Device / head | R/W | Drive select + LBA[24:27] or head |
| 0x1F7 / 0x177 | Status (R) / Command (W) | R/W | Status flags / command issue |
| 0x3F6 / 0x376 | Alt status (R) / Device control (W) | R/W | Non-interrupt status / reset + nIEN |

The IDE controller claims port 0x3F6 (shared region with the floppy
controller's 0x3F7).

## Bus master DMA

The controller provides PCI bus master DMA via BAR4. Each channel has
its own registers (primary at BAR4+0, secondary at BAR4+8):

| Offset | Register | Purpose |
|--------|----------|---------|
| +0 | Command | Start DMA, read/write direction |
| +2 | Status | Active, DMA error, interrupt flags |
| +4 | PRD table pointer | Physical Region Descriptor table address |

The PRD table is a scatter-gather list in guest memory. Each entry
contains a 32-bit physical base address, a 16-bit byte count, and an
end-of-table flag. DMA transfers iterate entries until the end-of-table
bit or the requested byte count is reached.

PCI config space includes PIIX4-specific timing registers
(`PRIMARY_TIMING_REG_ADDR` at 0x40, `SECONDARY_TIMING_REG_ADDR` at
0x44) and a UDMA control register (`UDMA_CTL_REG_ADDR` at 0x48).

## ATA hard drives

The ATA (AT Attachment) protocol defines a register-based command
interface for hard drives. The guest programs LBA, sector count, and
command into the command block registers, then transfers data via PIO
or DMA. The emulator implements the subset that OS drivers actually
use:

- Data transfer: `READ SECTORS`, `WRITE SECTORS` (PIO), `READ DMA`,
  `WRITE DMA` (DMA), plus 48-bit LBA extended variants.
- `WRITE DMA FUA EXT` — force unit access, mapped to
  `Disk::write_vectored` with `fua: true`.
- `IDENTIFY DEVICE` — returns 512 bytes of drive geometry,
  capabilities, and supported command sets.
- `FLUSH CACHE` / `FLUSH CACHE EXT` — mapped to `Disk::sync_cache`.
- `SET FEATURES`, `SET MULTI BLOCK MODE`, power management
  (`STANDBY`, `IDLE`, `SLEEP`, `CHECK POWER MODE`).

Commands not implemented (including SMART, security, and device
configuration overlays) return an error. The emulator doesn't emulate
PIO timing — transfers complete as fast as the backend can serve them.

## ATAPI optical drives

The ATAPI (ATA Packet Interface) extension transports SCSI commands
over the ATA register interface. The guest issues `PACKET COMMAND`
(0xA0), then writes a 12-byte SCSI CDB through the data register.
The controller forwards this CDB to
[`SimpleScsiDvd`](https://openvmm.dev/rustdoc/linux/scsidisk/scsidvd/struct.SimpleScsiDvd.html),
which handles optical-specific SCSI commands (READ,
GET_CONFIGURATION, START_STOP_UNIT for eject,
GET_EVENT_STATUS_NOTIFICATION for media change).

This layering means the ATAPI drive is a thin ATA-to-SCSI bridge —
the same `SimpleScsiDvd` implementation serves both StorVSP (direct
SCSI) and IDE (via ATAPI). See the
[storage pipeline — virtual optical / DVD](../../architecture/devices/storage.md#virtual-optical--dvd)
section for the DVD model and eject behavior.

`IDENTIFY PACKET DEVICE` (0xA1) returns device identification with
ATAPI-specific fields (general config word indicates removable media,
ATAPI device type).

## Enlightened I/O

The IDE controller supports a Microsoft-specific performance
optimization: enlightened INT13 commands. Instead of the guest issuing
a sequence of register writes to set up an ATA/ATAPI command (LBA,
sector count, command register, then DMA start), the guest writes a
single `EnlightenedInt13Command` packet to guest memory and writes
the packet's GPA to the enlightened port.

Enlightened ports: 0x1E0 (primary channel), 0x160 (secondary channel).

The `EnlightenedInt13Command` struct contains the ATA command opcode,
device/head select, full 48-bit LBA, block count, a GPA for the data
buffer, byte count, skip-bytes for partial sector transfers, and a
result status field written by the controller on completion.

This collapses the multi-exit register-programming sequence into a
single VM exit, significantly reducing overhead for legacy IDE I/O.
The enlightened path uses the same `DeferredWrite` async I/O
mechanism — the I/O port write returns deferred, and the controller
completes it when the disk operation finishes.

Both HDD and optical drives support enlightened commands, with
separate completion paths.

## Limitations

- No hot-add or hot-remove.
- No online disk resize (IDE has no capacity-change notification).
- Maximum four devices (two channels × two drives).
- No native command queuing (NCQ) — one command at a time per channel.

## Crate

[`ide`](https://openvmm.dev/rustdoc/linux/ide/index.html). See also
[`ide_resources`](https://openvmm.dev/rustdoc/linux/ide_resources/index.html)
for the `GuestMedia` enum and `IdeDeviceConfig` types. The
[storage pipeline](../../architecture/devices/storage.md) page covers
how IDE fits into the broader frontend-to-backend architecture.
