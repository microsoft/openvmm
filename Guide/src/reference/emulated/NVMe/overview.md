# NVMe Emulator

Among the devices that OpenVMM emulates, an NVMe controller is one. The OpenVMM NVMe emulator comes in two flavors:

- An NVMe emulator that can be used to serve IO workloads (but pragmatically is only used by OpenVMM for test scenarios today)
- An NVMe emulator used to test OpenHCL (`nvme_test`), which allows test authors to inject faults and inspect the state of NVMe devices used by the guest, and

This guide provides a brief overview of the architecture shared by the NVMe emulators.

```admonish note title="See also"
[Storage Pipeline](../../architecture/devices/storage.md) for the full frontend-to-backend architecture, including how NVMe namespaces map to `DiskIo` backends, online disk resize via AEN, and the layered disk model.
```
