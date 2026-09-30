# Virtio RTC

OpenVMM's `virtio_rtc` device provides a read-only, host-backed clock through
the [Virtio 1.4 RTC protocol][spec] (device ID 17).

## Clock and requests

The device exposes clock ID 0 with type `UTC_MAYBE_SMEARED`. Readings use
host `SystemTime` and are returned as little-endian 64-bit nanoseconds since
the Unix epoch. The host's leap-second policy is unspecified. Nanosecond
units do not imply nanosecond accuracy, and host clock adjustments can move
the reported time forward or backward.

Supported requests discover the clock count, query clock capabilities, and
read the clock. Cross-timestamp capability queries succeed with no supported
pairs. Cross-timestamp reads return `EOPNOTSUPP`. Alarms are not advertised,
there is no alarm queue, and alarm requests return `ENODEV`.

The implementation uses one request virtqueue and no device-specific
configuration registers. It follows the existing small-device worker
pattern, reusing OpenVMM's virtqueue, guest-memory, and task-control
infrastructure. Requests are processed sequentially. The device has no
mutable clock state and supports transport save/restore; reads after restore
return current host time. Additional clocks and optional features are
follow-up work.

## Configuration

- `--virtio-rtc` enables the device.
- `--virtio-rtc-bus <BUS>` selects `auto`, `mmio`, `pci`, `pcie:PORT`, or
  `vpci`; the default is `auto`.
- `--virtio-rtc-pcie-port <PORT>` attaches the device to a named PCIe port,
  requires `--virtio-rtc`, and overrides the bus selection.

See the [CLI reference](../../openvmm/management/cli.md) for device attachment
options.

## Guest support

The guest needs a Virtio RTC driver. Linux requires `CONFIG_VIRTIO_RTC` and
`CONFIG_VIRTIO_RTC_PTP` for this clock, together with the PTP subsystem.
Linux exposes it through the associated `/dev/ptpN` clock, read using
`clock_gettime()` with the device's dynamic clock ID, not through a legacy
CMOS RTC or the guest's system clock. Providing the device does not
automatically synchronize guest system time.

See the [`virtio_rtc` API documentation][rustdoc] for implementation details.

[spec]:
  https://docs.oasis-open.org/virtio/virtio/v1.4/virtio-v1.4.html#x1-86300023
[rustdoc]: https://openvmm.dev/rustdoc/virtio_rtc/index.html
