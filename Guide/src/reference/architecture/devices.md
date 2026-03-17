# Device architecture

This section covers the internal architecture of device emulators and
their backends — the shared machinery that both OpenVMM and OpenHCL
use to connect guest-visible storage, networking, and other devices to
their backing implementations.

## Pages

- [Storage pipeline](./devices/storage.md) — how guest I/O flows from
  a storage frontend (NVMe, SCSI, IDE) through the
  [`DiskIo`](https://openvmm.dev/rustdoc/linux/disk_backend/trait.DiskIo.html)
  abstraction to a concrete backing store.
