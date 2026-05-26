# OpenHCL VMBus Relay and Device Interception

OpenHCL owns the guest-facing VMBus control plane in VTL2. It decides whether to re-offer or intercept each host offer before VTL0 sees it.

## Overview

OpenHCL sits between the Root and the VTL0 guest OS, but it does not behave like a transparent wire. For VMBus devices, OpenHCL owns the VTL0 guest-facing VMBus server in VTL2. That means VTL0 talks to OpenHCL's VMBus server, not directly to the Root.

When VMBus redirection is enabled, OpenHCL can consume offers from the host side and decide what to do with them. Some offers are re-offered to VTL0 as relayed VMBus devices. Other offers are intercepted in VTL2 and used as backing for device-specific logic such as synthetic networking or storage translation.

## Three layers to keep separate

| Layer | What it means | Examples |
|-------|---------------|----------|
| **VMBus transport** | The guest-facing synthetic bus protocol and channel model. | Offers, channels, interrupts, GPADL-related machinery |
| **VMBus relay / redirect** | Host offers are consumed in VTL2 and re-offered to VTL0 through the OpenHCL-owned VMBus server. | `HostVmbusTransport`, `--vmbus-redirect`, `vmbus_redirection_enabled` |
| **Device interception / translation** | A selected offer is filtered out of the relayed set and routed into VTL2-owned device logic. | MANA to synthetic NIC, NVMe backing to storvsp-backed SCSI |

VPCI sits next to those layers as a specialized sibling path. VPCI-capable devices still show up on the Root-to-VTL2 boundary as VMBus offers, but the OpenHCL worker does not leave them in the general relay stream. It builds a dedicated `vpci` filter and a separate general `relay` filter, then routes the VPCI-filtered offers into `VpciRelay` while the remaining offers go through `HostVmbusTransport`. That is why VPCI filtering does matter here, but VPCI relay is not just "the normal VMBus relay for one more device type."

The rest of the page keeps those layers separate. VMBus relay is the transport and control-plane story. Storage translation and synthetic networking are device-specific stories built on top of it. For broader process-level context around where this runs in VTL2, see [OpenHCL Architecture](../openhcl.md) and [OpenHCL Processes and Components](./processes.md).

## High-level stack

```text
  ┌──────────────────────────────────────┐
  │  VTL0 guest                          │
  │                                      │
  │  Guest OS                            │
  │    └─ VMBus devices                  │
  │       (storvsp, netvsp, serial, ...) │
  └──────────────────┬───────────────────┘
                     │
  ┌──────────────────┼───────────────────┐
  │  VTL2 OpenHCL    │                   │
  │                  ▼                   │
  │  VMBus server                        │
  │    ├─ Host-offer relay               │
  │    └─ VTL2-owned device logic        │
  └──────────────────┬───────────────────┘
                     │
  ┌──────────────────┼───────────────────┐
  │  Root / host     │                   │
  │                  ▼                   │
  │  Host backends ─► Host VMBus offers  │
  └──────────────────────────────────────┘
```

The key boundary is the guest-facing server in VTL2. Every guest-visible VMBus device is surfaced through that server, even when the backing side started as a host offer.

## Relay versus interception

```text
  Host VMBus offer
   │
   ├─ Relay ──────► Re-offer the device to VTL0
   │                Guest sees a relayed VMBus device
   │
   └─ Intercept ──► Route into VTL2-owned device logic
                    Guest sees a device owned by OpenHCL
```

This is the simplest way to think about the split:

- In the **relay** case, OpenHCL keeps ownership of the guest-facing VMBus control plane but re-offers the device to VTL0.
- In the **intercept** case, OpenHCL does not pass that offer through unchanged. Instead, it routes the offer into VTL2-owned logic that decides what the guest should see.

The guest can still see a VMBus device in both cases, but the ownership model is different.

## Control plane versus data plane

| Aspect | Relay case | Intercepted / translated case |
|--------|------------|-------------------------------|
| **Who owns the guest-facing VMBus control plane?** | OpenHCL in VTL2 | OpenHCL in VTL2 |
| **What happens to the host offer?** | It is consumed in VTL2 and re-offered to VTL0. | It is consumed in VTL2 and routed into VTL2-owned device logic. |
| **Where does device-specific behavior live?** | Mostly outside the relay itself. | In the VTL2 device implementation. |
| **Does OpenHCL stay out of the data plane?** | That is the intended pure-relay model described by `vmbus_relay`. | No. Once OpenHCL owns the device semantics, VTL2 is part of the device path. |

The control-plane story is stable across both columns: OpenHCL is the VMBus server. The data-plane story changes when OpenHCL starts owning device semantics instead of only relaying host channels.

## What relay means in this codebase

The `vmbus_relay` crate gives the clearest definition of relay. It consumes channels from the host VMBus control plane and relays them to the guest through `vmbus_server`. In the pure relay case, it keeps the paravisor out of the data plane.

That is narrower than "OpenHCL translates devices." Relay describes how offers and channels cross the Root-to-VTL2-to-VTL0 boundary. Translation describes what a particular device does after OpenHCL decides to own its semantics in VTL2.

## Concrete OpenVMM examples

The [Running OpenHCL with OpenVMM](../../../user_guide/openhcl/run/openvmm.md) page shows the same layering in a compact surface:

```bash
# Turn on host-offer relay support
--vmbus-redirect

# Offer MANA into VTL2 and expose a synthetic NIC to VTL0
--net uh:consomme --vmbus-redirect

# Offer NVMe into VTL2 and expose storvsp-backed SCSI to VTL0
--disk mem:1G,uh-nvme --vmbus-redirect
```

These examples show three separate decisions:

1. `--vmbus-redirect` enables the host-offer relay substrate.
2. `uh:*` or `uh-nvme` determines what the Root offers into VTL2.
3. The resulting guest-visible device family is still chosen by VTL2-owned device logic, not by the existence of relay alone.

```admonish note title="How VPCI relay differs from the general VMBus relay path"
VPCI-capable devices still arrive as VMBus offers, which is why VPCI relay depends on VMBus relay being active at all. But once OpenHCL identifies those offers, it does not hand them to the generic `HostVmbusTransport` path. Instead, it routes them into `VpciRelay`, which handles the extra PCI-visible semantics such as allowed-device filtering and MMIO space. In other words, VPCI relay starts from VMBus offers but does different work after that handoff point.
```

## Where synthetic networking fits

Synthetic networking is the other main consumer of device interception. The [architecture overview](../openhcl.md) shows how OpenHCL translates between MANA (Microsoft Azure Network Adapter) on the backing side and a synthetic VMBus NIC on the guest-visible side.

The bus-level mechanics are the same as storage interception:

1. The Root offers a MANA NIC into VTL2.
2. OpenHCL intercepts the offer instead of relaying it to VTL0.
3. OpenHCL exposes a synthetic VMBus NIC to VTL0 through the guest-facing VMBus server.

The guest sees a standard VMBus network device and loads the normal netvsp/netvsc driver stack. OpenHCL handles the translation between the guest-facing synthetic protocol and the backing MANA hardware interface.

This is the `--net uh:consomme --vmbus-redirect` path shown in [Running OpenHCL with OpenVMM](../../../user_guide/openhcl/run/openvmm.md). In Azure, this translation allows unmodified guests to use hardware-accelerated networking without needing a MANA-aware driver.

## Where storage translation fits

Storage translation is one consumer of this transport model. The bus-level story says that OpenHCL owns the guest-facing VMBus server and can consume host offers in VTL2. The storage-specific story says that OpenHCL can then expose a guest-visible storage controller and disks whose semantics differ from the backing device family offered into VTL2.

This distinction matters when reading the storage pages. In this page, "relay" names the bus and control-plane behavior. In the storage pages, "storage translation" names the controller, disk, namespace, and backing-device mapping that sits on top of that relay substrate.

## Implementation map

| Component | Why read it | Source | Rustdoc |
|-----------|-------------|--------|---------|
| `vmbus_server` | The guest-facing VMBus server that VTL0 talks to | [`vm/devices/vmbus/vmbus_server/src/lib.rs`](https://github.com/microsoft/openvmm/tree/main/vm/devices/vmbus/vmbus_server/src/lib.rs) | [`vmbus_server`](https://openvmm.dev/rustdoc/linux/vmbus_server/index.html) |
| `vmbus_relay` | Defines the pure relay model and `HostVmbusTransport` | [`vm/devices/vmbus/vmbus_relay/src/lib.rs`](https://github.com/microsoft/openvmm/tree/main/vm/devices/vmbus/vmbus_relay/src/lib.rs) | [`vmbus_relay`](https://openvmm.dev/rustdoc/linux/vmbus_relay/index.html) |
| `vmbus_relay_intercept_device` | Shows how a device is filtered out of the relayed offer list and terminated in VTL2-owned logic | [`vm/devices/vmbus/vmbus_relay_intercept_device/src/lib.rs`](https://github.com/microsoft/openvmm/tree/main/vm/devices/vmbus/vmbus_relay_intercept_device/src/lib.rs) | [`vmbus_relay_intercept_device`](https://openvmm.dev/rustdoc/linux/vmbus_relay_intercept_device/index.html) |
| OpenHCL worker wiring | Shows how VMBus redirection is enabled and how relay is wired into the runtime | [`openhcl/underhill_core/src/worker.rs`](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_core/src/worker.rs) | [`underhill_core`](https://openvmm.dev/rustdoc/linux/underhill_core/index.html) |
| Relay lifecycle management | Shows that relay is a managed state unit with start, save, and restore handling | [`openhcl/underhill_core/src/vmbus_relay_unit.rs`](https://github.com/microsoft/openvmm/tree/main/openhcl/underhill_core/src/vmbus_relay_unit.rs) | [`underhill_core`](https://openvmm.dev/rustdoc/linux/underhill_core/index.html) |
