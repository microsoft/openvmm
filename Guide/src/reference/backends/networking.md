# Networking backends

The networking backend system connects guest-facing NICs (frontends)
to host-side packet I/O (backends) through a shared trait interface
defined in the `net_backend` crate.

## Architecture overview

```text
┌─────────────┐  ┌──────────────┐  ┌─────────────┐
│  virtio_net  │  │    netvsp     │  │  gdma/bnic  │
│  (frontend)  │  │  (frontend)   │  │  (frontend) │
└──────┬───────┘  └──────┬────────┘  └──────┬──────┘
       │                 │                   │
       │    &mut dyn BufferAccess            │
       │    (owned by frontend)              │
       │                 │                   │
       ▼                 ▼                   ▼
┌──────────────────────────────────────────────────┐
│              dyn Queue  (per-queue)               │
│  poll_ready · rx_avail · rx_poll                  │
│  tx_avail · tx_poll                               │
└──────────────────────────────────────────────────┘
       ▲                 ▲                   ▲
       │                 │                   │
┌──────┴───┐  ┌──────────┴───┐  ┌────────────┴───┐
│ TapQueue │  │ConsommeQueue │  │   ManaQueue    │
│ DioQueue │  │LoopbackQueue │  │  (hardware)    │
│  ...     │  │  NullQueue   │  │                │
└──────────┘  └──────────────┘  └────────────────┘
```

There are three layers:

- **Frontend** — the guest-visible NIC device (virtio-net, netvsp, or
  GDMA/BNIC). Owns the `BufferAccess` implementation and drives the
  poll loop.
- **Queue** — a single TX/RX data path. Backends implement the
  `Queue` trait. A device may have multiple queues for RSS.
- **Endpoint** — a backend factory. One per NIC. Creates `Queue`
  objects when the frontend activates the device.

## Key traits

### `Endpoint`

A backend factory. Frontends call `get_queues()` to create `Queue`
objects for each RSS queue, then `stop()` on teardown.

```rust
// Simplified — see net_backend::Endpoint for the full trait.
trait Endpoint {
    async fn get_queues(
        &mut self,
        config: Vec<QueueConfig>,
        rss: Option<&RssConfig<'_>>,
        queues: &mut Vec<Box<dyn Queue>>,
    ) -> anyhow::Result<()>;

    async fn stop(&mut self);
}
```

### `Queue`

A single data-path queue. All methods that touch receive buffers
take `pool: &mut dyn BufferAccess` as the first data parameter
(after `cx` for `poll_ready`):

```rust
// Simplified — see net_backend::Queue for the full trait.
trait Queue {
    fn poll_ready(&mut self, cx, pool) -> Poll<()>;
    fn rx_avail(&mut self, pool, done: &[RxId]);
    fn rx_poll(&mut self, pool, packets: &mut [RxId]) -> Result<usize>;
    fn tx_avail(&mut self, pool, segments: &[TxSegment]) -> Result<(bool, usize)>;
    fn tx_poll(&mut self, pool, done: &mut [TxId]) -> Result<usize>;
}
```

### `BufferAccess`

Provides guest-memory access for receive buffers. Implemented by the
frontend (e.g., `VirtioWorkPool` for virtio-net, `BufferPool` for
netvsp, `GuestBuffers` for gdma). The frontend owns this and passes
`&mut` references into `Queue` methods — no `Arc` or `Mutex` needed.

```rust
trait BufferAccess {
    fn guest_memory(&self) -> &GuestMemory;
    fn push_guest_addresses(&self, id: RxId, buf: &mut Vec<RxBufferSegment>);
    fn capacity(&self, id: RxId) -> u32;
    fn write_data(&mut self, id: RxId, data: &[u8]);
    fn write_header(&mut self, id: RxId, metadata: &RxMetadata);
}
```

## Lifecycle

1. Frontend creates a `BufferAccess` and one `QueueConfig` per queue.
2. Calls `endpoint.get_queues(configs, rss, &mut queues)`.
3. Posts initial receive buffers:
   `queue.rx_avail(&mut pool, &initial_rx_ids)`.
4. Enters poll loop: `poll_ready` → `rx_poll` / `tx_avail` / `tx_poll`.
5. On shutdown, drops queues and calls `endpoint.stop()`.

## Backend catalog

| Backend | Crate | Transport | Platform |
|---------|-------|-----------|----------|
| TAP | `net_tap` | Linux TAP device | Linux |
| DirectIO | `net_dio` | Windows vmswitch | Windows |
| Consomme | `net_consomme` | User-space TCP/IP stack | Cross-platform |
| MANA | `net_mana` | Azure hardware NIC (MANA/GDMA) | Linux (VFIO) |
| Loopback | `net_backend::loopback` | Reflects TX → RX | Cross-platform |
| Null | `net_backend::null` | Drops everything | Cross-platform |

### Wrappers

| Wrapper | Crate | Purpose |
|---------|-------|---------|
| PacketCapture | `net_packet_capture` | PCAP tracing (wraps inner endpoint) |
| Disconnectable | `net_backend` | Hot-plug/unplug (wraps inner endpoint) |

## Frontend catalog

| Frontend | Crate | Guest interface |
|----------|-------|-----------------|
| virtio-net | `virtio_net` | Virtio network device (virtqueue-based) |
| netvsp | `netvsp` | VMBus synthetic NIC (Windows/Linux guests) |
| GDMA/BNIC | `gdma` | MANA Basic NIC (emulated GDMA hardware) |

## BufferAccess implementations

Each frontend has its own `BufferAccess` implementation that maps
`RxId` values to guest memory:

| Type | Crate | Notes |
|------|-------|-------|
| `VirtioWorkPool` | `virtio_net` | Wraps virtio descriptor chains |
| `BufferPool` | `netvsp` | Maps into VMBus receive buffer GPADL |
| `GuestBuffers` | `gdma` | Maps GDMA receive WQEs |

## Ownership model

The core design principle: **the frontend owns `BufferAccess`**.

The frontend holds the `BufferAccess` as a plain field (no `Arc`, no
`Mutex`) and passes `&mut dyn BufferAccess` into each `Queue` method
call. This is possible because:

- `Queue` methods are called from a single async task per queue.
- The backend never stores a reference to `BufferAccess` — it uses
  the reference only for the duration of the method call.
- `push_guest_addresses` takes `&self` (not `&mut self`) and appends to
  a caller-provided `Vec`, so it can be called alongside
  `guest_memory()` without borrow conflicts.

This eliminates the per-packet `Mutex` locks that were previously
needed when `BufferAccess` was boxed and stored inside the `Queue`.
