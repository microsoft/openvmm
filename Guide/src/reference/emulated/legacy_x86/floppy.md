# Floppy

The floppy controller emulates an
[Intel 82077AA](https://en.wikipedia.org/wiki/Intel_82077AA) CHMOS
single-chip floppy disk controller. It connects to the storage stack
through
[`Disk`](https://openvmm.dev/rustdoc/linux/disk_backend/struct.Disk.html)
— the same backend abstraction used by NVMe and SCSI. Data transfers
use ISA DMA channel 2; interrupts use IRQ 6.

Two variants exist:

- [`FloppyDiskController`](https://openvmm.dev/rustdoc/linux/floppy/struct.FloppyDiskController.html)
  — full emulator with disk I/O.
- [`StubFloppyDiskController`](https://openvmm.dev/rustdoc/linux/floppy_pcat_stub/struct.StubFloppyDiskController.html)
  — reports "no drives" for PCAT BIOS compatibility when no floppy is
  configured.

## Supported media

The controller auto-detects the floppy format from the disk image byte
size. See
[Wikipedia's list of floppy disk formats](https://en.wikipedia.org/wiki/List_of_floppy_disk_formats)
for background on these formats.

| Format | Capacity | Sectors/track | Notes |
|--------|----------|---------------|-------|
| Low density (SS) | 360 KB | 9 | Single-sided (one head) |
| Low density | 720 KB | 9 | |
| Medium density | 1.2 MB | 15 | |
| High density | 1.44 MB | 18 | Most common format |
| [DMF](https://en.wikipedia.org/wiki/Distribution_Media_Format) | 1.68 MB | 21 | Microsoft Distribution Media Format |
| XDF | 1.72 MB | 23 | Extended density (fixed 23 SPT variant) |

All formats use 512-byte sectors, 80 cylinders, CHS addressing. The
controller rejects images that don't match a known format size.

## I/O port layout

Register offsets from base (typically 0x3F0):

| Offset | Read | Write | Purpose |
|--------|------|-------|---------|
| +0 | STATUS_A | — | Fixed 0xFF (not emulated) |
| +1 | STATUS_B | — | Fixed 0xFC (no tape drives) |
| +2 | DOR | DOR | Motor control, drive select, DMA gate, reset |
| +4 | MSR | DSR | Main status (busy, direction, RQM) / data rate select |
| +5 | DATA | DATA | Command/parameter/result FIFO (16-byte) |
| +7 | DIR | CCR | Disk change signal / config control |

The controller claims port 0x3F7 for DIR/CCR separately from the
6-byte base region, because 0x3F6 is shared with the IDE controller's
alternate status register.

## Limitations and deviations

The real 82077AA supports four drives; OpenVMM supports one. The
emulator implements a pragmatic subset of the command set — enough for
MS-DOS, Windows, and Linux floppy drivers to detect the controller,
identify media, and perform read/write/format operations. Commands that
interact with physical media timing (perpendicular recording mode,
power management) are accepted but largely no-op'd.

Key differences from real hardware:

- No multi-drive support (real hardware supports drives 0–3).
- Physical media timing (step rate, head load/unload from SPECIFY) is
  accepted but doesn't affect I/O timing.
- CHS-to-LBA translation is straightforward — the controller doesn't
  emulate track-level interleave or skew.
- STATUS_A and STATUS_B registers return fixed values rather than reflecting physical drive state.

## Crates

| Crate | Purpose | Rustdoc |
|-------|---------|---------|
| `floppy` | Full 82077AA emulator | [rustdoc](https://openvmm.dev/rustdoc/linux/floppy/index.html) |
| `floppy_pcat_stub` | Stub controller (no drives) | [rustdoc](https://openvmm.dev/rustdoc/linux/floppy_pcat_stub/index.html) |
| `floppy_resources` | Config types (Resource-based instantiation not yet implemented) | [rustdoc](https://openvmm.dev/rustdoc/linux/floppy_resources/index.html) |

The [storage pipeline](../../architecture/devices/storage.md) page covers how the floppy controller connects to the broader disk backend abstraction.
