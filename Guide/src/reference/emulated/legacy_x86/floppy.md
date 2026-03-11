# Floppy

The floppy controller emulates a legacy ISA floppy disk controller.

## Overview

The floppy controller exists for compatibility with legacy boot scenarios and older guest operating systems. It is rarely used in modern configurations.

## Crates

- `floppy/` — the floppy controller emulator.
- `floppy_pcat_stub/` — a stub implementation for PCAT configurations where a floppy controller must be present in the chipset but no media is attached.
- `floppy_resources/` — resource definitions for floppy configuration.
