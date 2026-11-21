# Virtio RTC

OpenVMM's `virtio_rtc` device provides a read-only, host-backed clock through
the [Virtio 1.4 RTC protocol][spec] (device ID 17).

## Clock and requests

The device exposes clock ID 0 with type `UTC_SMEARED` and an unspecified
smearing variant. Readings use host `SystemTime` and are returned as
little-endian 64-bit nanoseconds since the Unix epoch. No device-side
smearing is applied. Nanosecond units do not imply nanosecond accuracy, and
host clock adjustments can move the reported time forward or backward.

Supported requests discover the clock count, query clock capabilities, and
read the clock. Cross-timestamp capability queries succeed with no supported
pairs. Cross-timestamp reads return `EOPNOTSUPP`. Alarms are not advertised,
there is no alarm queue, and alarm requests return `ENODEV`.

The device uses one request virtqueue and no device-specific configuration
registers. Requests are processed sequentially. The device supports
transport save/restore; reads after restore return current host time.
Additional clocks and optional features are follow-up work.

## Configuration

- `--virtio-rtc` enables the device with automatic bus selection.
- `--virtio-rtc bus=BUS` selects `auto`, `mmio`, `pci`, `pcie:PORT`, or
  `vpci`. For example, `--virtio-rtc bus=pcie:rp0` attaches it to the named
  PCIe port `rp0`.

The device is disabled when the option is absent. Omitting `bus` preserves
the default `auto` selection: VPCI on Windows/macOS with Hyper-V
enlightenments enabled, and PCI otherwise.

See the [CLI reference](../../openvmm/management/cli.md) for device attachment
options.

The gRPC/ttrpc management API also accepts the fieldless `VirtioRtc` message.
It supports boot-time PCIe attachment through `CreateVM` and the existing
`AddPcieDevice` and `AddVpciDevice` paths, subject to their platform and
topology requirements.

## Guest support

Linux can register an RTC-class device with `CONFIG_VIRTIO_RTC_CLASS`.
For kernel boot-time wall-clock initialization, build in the driver with
`CONFIG_VIRTIO_RTC=y`, enable `CONFIG_RTC_HCTOSYS`, and set
`CONFIG_RTC_HCTOSYS_DEVICE` to the assigned RTC name (for example, `rtc0`).
With `CONFIG_VIRTIO_RTC_PTP` and the PTP subsystem, Linux also exposes the
clock through `/dev/ptpN`, read using `clock_gettime()` with the device's
dynamic clock ID.

[spec]: https://docs.oasis-open.org/virtio/virtio/v1.4/virtio-v1.4.html
