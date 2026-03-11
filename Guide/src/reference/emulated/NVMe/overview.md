# NVMe emulator

Among the devices that OpenVMM emulates, an NVMe controller is one. The OpenVMM NVMe emulator comes in two flavors:

- An NVMe emulator that can be used to serve IO workloads (but pragmatically is only used by OpenVMM for test scenarios today)
- An NVMe emulator used to test OpenHCL ([`nvme_test`](https://openvmm.dev/rustdoc/linux/nvme_test/index.html)), which allows test authors to inject faults and inspect the state of NVMe devices used by the guest.

This guide provides a brief overview of the architecture shared by the NVMe emulators. For how NVMe fits into the broader storage pipeline — including how namespaces map to [`DiskIo`](https://openvmm.dev/rustdoc/linux/disk_backend/trait.DiskIo.html) backends, online disk resize via AEN, and the layered disk model — see the [storage pipeline](../../architecture/devices/storage.md) page.
