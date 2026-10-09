# IPMI KCS

OpenVMM provides a minimal virtual IPMI baseboard management controller (BMC)
using the Keyboard Controller Style (KCS) system interface. The device allows
UEFI guests to write and query a bounded System Event Log (SEL).

The implementation is provided by the `ipmi_kcs` crate. Wire-level command and
record definitions are in the `ipmi_protocol` crate.

## Configuration

The device is created when the platform configuration enables IPMI. OpenHCL
accepts this setting only for UEFI guests; PCAT and Linux-direct boot modes are
not supported.

The guest-visible register interface depends on the architecture:

| Architecture | Interface | Registers |
| --- | --- | --- |
| x86-64 | Port I/O | Data at `0xCA2`; status/command at `0xCA3` |
| AArch64 | MMIO | Data at `0xEFFE7000`; status/command at `0xEFFE7004` |

Only one-byte register accesses are supported.

## Supported commands

The virtual BMC implements the IPMI application `Get Device ID` command and
these SEL storage commands:

- Get SEL Info
- Reserve SEL
- Get SEL Entry
- Add SEL Entry
- Clear SEL
- Get SEL Time
- Set SEL Time

The SEL stores at most 128 records. Reservations are exposed for guest software
compatibility but are not enforced because the device has one serialized KCS
requestor and no independent SEL mutators.

Completed SEL records are retained by the device and forwarded to the host on
a best-effort basis. Forwarding is limited to 256 records per trusted
wall-clock second; reaching that limit does not remove records from the SEL.

## Servicing

Saved state includes the KCS transaction, SEL records, record allocation state,
reservation identifier, and guest-selected SEL time offset. Diagnostic
forwarding counters and rate-limiter state are reset after restore.

See the
[`ipmi_kcs` rustdoc](https://openvmm.dev/rustdoc/ipmi_kcs/index.html) for the
device API.
