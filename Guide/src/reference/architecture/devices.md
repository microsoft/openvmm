# Device Architecture

This section covers the internal architecture of device emulators and their backends — the shared machinery that both OpenVMM and OpenHCL use to connect guest-visible storage, networking, and other devices to their backing implementations.

## Pages

- [Storage Pipeline](./devices/storage.md) — how guest I/O flows from a storage frontend (NVMe, SCSI, IDE) through the disk backend abstraction to a concrete backing store.
