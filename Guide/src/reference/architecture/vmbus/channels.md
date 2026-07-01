# VMBus Channels

VMBus is the synthetic bus that connects guest drivers to host-side device
backends. Every VMBus device communicates through one or more **channels** —
bidirectional ring-buffer pairs backed by guest memory.

## What is a channel?

A VMBus channel is:

- A **ring buffer pair** — one incoming (guest → host), one outgoing (host →
  guest) — backed by a single guest-allocated GPADL (Guest Physical Address
  Descriptor List) — a guest-provided description of guest-physical pages
  shared with the host. "Incoming" and "outgoing" are always relative to the
  local endpoint: each side's incoming ring is the other side's outgoing ring.
  In OpenVMM (the host), the incoming ring carries data from the guest and
  the outgoing ring carries data to the guest.
- An **interrupt/event signal** for each direction.
- A **target VP** — the guest vCPU targeted for channel notifications. In
  OpenVMM's current implementation, this value also selects the host-side
  executor used for processing that channel.

Each channel is identified by a unique `channel_id` assigned by the VMBus server
at offer time. The channel's lifecycle is: **offered → opened → closed** (or
**rescinded** by the host). If the host rescinds an offer, the channel is torn
down regardless of guest state.

```text
  ┌──────────────────────────────────────────────────┐
  │  VMBus Channel                                   │
  │                                                  │
  │  ┌───────────────────┐  ┌───────────────────┐    │
  │  │  Incoming Ring    │  │  Outgoing Ring    │    │
  │  │  (guest → host)   │  │  (host → guest)   │    │
  │  └─────────┬─────────┘  └─────────┬─────────┘    │
  │            │                      │              │
  │  ┌─────────┴──────────────────────┴─────────┐    │
  │  │  GPADL-backed memory (guest-allocated)   │    │
  │  └──────────────────────────────────────────┘    │
  │                                                  │
  │  Signal: guest → host    Signal: host → guest    │
  │  Target VP: set at open time                     │
  └──────────────────────────────────────────────────┘
```

## Subchannels

A **subchannel** is a full additional VMBus channel offer for the same device
instance. It is not a side-queue or a sub-object of the primary channel — it has
its own ring buffer GPADL, its own open/close lifecycle, its own channel ID, and
its own target VP.

The identity of a channel within a device is the tuple `(interface_id,
instance_id, subchannel_index)`:

| Field | Meaning |
|-------|---------|
| `interface_id` | Device type GUID (e.g., SCSI controller) |
| `instance_id` | Specific device instance |
| `subchannel_index` | `0` for the primary channel, `1..n` for subchannels |

### Primary and subchannel relationship

- The **primary channel** (`subchannel_index == 0`) is always offered first and
  handles protocol negotiation.
- **Subchannels** are offered only after the primary is open, when the device
  explicitly enables them.
- A subchannel **cannot exist without its primary channel**. If the primary
  channel closes, all subchannels are automatically revoked and closed.
- Subchannels are opened and closed independently; closing one subchannel does
  not inherently require closing the primary or other subchannels.

```mermaid
stateDiagram-v2
    [*] --> PrimaryOffered: VMBus server offers device
    PrimaryOffered --> PrimaryOpen: Guest opens primary (subchannel_index=0)
    PrimaryOpen --> SubchannelsOffered: Device backend requests N subchannels
    SubchannelsOffered --> AllOpen: Guest opens subchannels 1..n
    AllOpen --> PrimaryOpen: Guest closes subchannels
    PrimaryOpen --> [*]: Guest closes primary → all subchannels revoked
```

### Why subchannels exist

Subchannels enable **I/O parallelism with CPU locality**. Each channel has its
own ring buffer and target VP, so:

- Multiple VPs can issue I/O concurrently without contending on a single ring
  buffer.
- Each channel's host-side worker runs on the target VP's thread, keeping cache
  lines warm and avoiding cross-VP interrupts.

Without subchannels, all I/O for a device funnels through one ring and one
worker — a bottleneck on multi-VP VMs.

## Target VP

When a guest opens a channel, it specifies a `target_vp` — the guest vCPU that
will receive channel interrupts and events. In OpenVMM's current implementation,
the VMBus server also uses this value to select the executor that runs the device
worker for that channel.

The guest can change the target VP at runtime via the `ModifyChannel` VMBus
message. This is used when VPs come online/offline (e.g., CPU hot-remove) and
the guest needs to rebalance channel assignments.

If you're curious to learn more about how the VMM and guest decide on the notion
of a `target_vp`, see the [Processors](../concepts/procs.md) page.

## Ring buffer model

Each ring is a fixed-size circular buffer. The size is determined at channel
open time and cannot change while the channel is open. Key properties:

- **No overflow** — if the ring is full, the sender must wait. The full ring
  itself is the only backpressure mechanism; there is no explicit flow-control
  protocol.
- **Batched reads** — the host reads packets in batches via
  [`poll_read_batch()`](https://openvmm.dev/rustdoc/linux/vmbus_async/queue/struct.ReadHalf.html#method.poll_read_batch)
  (interrupt-driven) or
  [`try_read_batch()`](https://openvmm.dev/rustdoc/linux/vmbus_async/queue/struct.ReadHalf.html#method.try_read_batch)
  (poll mode, no interrupt).
- **Paired** — rings always come in pairs (incoming + outgoing). A channel
  without both rings is not usable.

Since ring buffers reside in guest-allocated memory, the host must treat all ring
contents as untrusted input.

For the ring buffer implementation, see the [`vmbus_ring`
rustdoc](https://openvmm.dev/rustdoc/linux/vmbus_ring/index.html).

## Key types

The following Rust types are the primary building blocks in OpenVMM's VMBus
implementation; device backends typically interact with `VmbusDevice`,
`ChannelControl`, and `Queue`.

| Type | Crate | Role |
|------|-------|------|
| `OfferKey` | `vmbus_channel` | Channel identity tuple |
| `OfferParams` | `vmbus_channel` | Full offer metadata |
| `OpenData` | `vmbus_channel` | Guest-provided open parameters (target VP, ring GPADL) |
| `ChannelControl` | `vmbus_channel` | Device-side handle to enable subchannels |
| `VmbusDevice` | `vmbus_channel` | Trait for VMBus device implementations |
| `RawAsyncChannel` | `vmbus_channel` | Async wrapper around a ring buffer pair |
| `IncomingRing` / `OutgoingRing` | `vmbus_ring` | Low-level ring buffer types |
| `Queue` | `vmbus_async` | High-level async packet read/write over a channel |
