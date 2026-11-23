# Networking backends

The networking backend system connects guest-facing NICs (frontends)
to host-side packet I/O (backends) through a shared trait interface
defined in the `net_backend` crate. This page explains how the
pieces fit together, how packets flow, and how to navigate the code.

## Architecture overview

```text
┌──────────────┐  ┌──────────────┐  ┌──────────────┐
│ virtio_net   │  │   netvsp     │  │  gdma/bnic   │
│ (frontend)   │  │ (frontend)   │  │ (frontend)   │
└───────┬──────┘  └───────┬──────┘  └───────┬──────┘
        │                 │                 │
        │  &mut dyn BufferAccess            │
        │  (owned by frontend)              │
        │                 │                 │
        ▼                 ▼                 ▼
┌──────────────────────────────────────────────────┐
│  dyn Queue  (per-queue)                          │
│  poll_ready · rx_avail · rx_poll                 │
│  tx_avail · tx_poll                              │
└──────────────────────────────────────────────────┘
        ▲                 ▲                 ▲
        │                 │                 │
┌───────┴──────┐  ┌───────┴──────┐  ┌───────┴──────┐
│ TapQueue     │  │ConsommeQueue │  │  ManaQueue   │
│ DioQueue     │  │LoopbackQueue │  │ (hardware)   │
│   ...        │  │  NullQueue   │  │              │
└──────────────┘  └──────────────┘  └──────────────┘
```

There are three layers:

- **Frontend** — the guest-visible NIC device (`virtio_net`,
  `netvsp`, or `gdma`). Owns the `BufferAccess` implementation
  (no `Arc` or `Mutex` needed — each queue is driven from a single
  async task), translates between the guest-specific descriptor
  format and the generic `Queue` interface, and drives the poll
  loop.

- **Queue** — a single TX/RX data path created by the backend.
  Frontends interact with it entirely through the
  [`Queue`](https://openvmm.dev/rustdoc/net_backend/trait.Queue.html)
  trait. A device may have multiple queues for RSS.

- **Endpoint** — a backend factory. One per NIC. The frontend calls
  [`Endpoint::get_queues`](https://openvmm.dev/rustdoc/net_backend/trait.Endpoint.html#tymethod.get_queues)
  when the guest activates the NIC and
  [`Endpoint::stop`](https://openvmm.dev/rustdoc/net_backend/trait.Endpoint.html#tymethod.stop)
  on teardown.

See the
[`net_backend` rustdoc](https://openvmm.dev/rustdoc/net_backend/)
for the full trait signatures and type definitions.

## Packet flow

### Transmit (guest → host)

1. The guest posts a TX descriptor (e.g. a virtio descriptor chain
   or a VMBus RNDIS message).
2. The frontend reads the descriptor from guest memory, extracts any
   offload metadata (checksum, TSO, USO), and builds a `TxSegment` array.
   Each segment carries a guest physical address and a length — **no
   data is copied** at this point.
3. The frontend calls `queue.tx_avail(&mut pool, &segments)`. The
   backend reads data directly from guest memory via
   `pool.guest_memory()` and transmits it (e.g. writes to a TAP fd,
   posts to hardware, or feeds it to a user-space TCP stack).
4. If the backend completes synchronously (`tx_avail` returns
   `sync = true`), the frontend can immediately mark the descriptor
   done. Otherwise, it polls `tx_poll` later for async completions.

### Receive (host → guest)

1. The frontend pre-populates the backend with receive buffers by
   calling `queue.rx_avail(&mut pool, &buffer_ids)`.
2. When `queue.poll_ready(cx, &mut pool)` signals readiness, the
   backend has received a packet. It writes the packet data into
   guest memory through `pool.write_packet(rx_id, metadata, data)`.
3. The frontend calls `queue.rx_poll(&mut pool, &mut ids)` to
   collect the IDs of completed buffers, then delivers them to the
   guest (e.g. by completing virtio descriptors or sending VMBus
   completion packets).
4. The guest eventually returns the buffer, and the frontend recycles
   it via `rx_avail`.

### Guest memory access

The `Queue` interface works with guest physical addresses rather
than host buffers, giving each backend flexibility in how it
accesses packet data. The patterns fall into three categories:

**GPA pass-through (hardware DMA).** `net_mana` converts guest
physical addresses into IO virtual addresses (`GuestMemory::iova`)
and posts them as scatter-gather entries directly to GDMA hardware.
The NIC DMAs packet data to/from guest memory without any host-side
copy. This is the fastest path, but requires IOMMU mappings and
contiguous-enough buffers; when those conditions aren't met, MANA
falls back to bounce buffers.

**Host-mediated copy.** Software backends like `net_consomme` and
`net_dio` read TX data from guest memory with
`GuestMemory::read_at`, process or forward it, and write RX data
back with `BufferAccess::write_packet`. The data passes through
host memory, but the `Queue` interface avoids any extra copies
between the frontend and backend layers — the backend reads/writes
guest RAM directly.

### Receive offload invariants

Receive checksum and segmentation offloads must be enabled end to end.
The frontend reports only capabilities negotiated with the guest in
`QueueConfig::enabled_rx_offloads`; the backend must not enable additional
host transport offloads.

This is especially important for Linux TAP. Enabling `TUN_F_CSUM`,
`TUN_F_TSO4`, or `TUN_F_TSO6` changes the packet format returned by the
TAP file descriptor: frames can carry partial-checksum metadata and can
be much larger than the configured network MTU. A frontend that did not
negotiate the corresponding guest feature cannot safely represent those
frames.

Linux TAP is configured with only the receive offloads reported by the
frontend. The resulting vnet header is therefore a trusted kernel contract:
the TAP backend translates it directly instead of reparsing the Ethernet,
IP, and transport headers. The backend also relies on Linux TAP to return a
complete frame no larger than `ETH_MAX_MTU` plus the Ethernet VLAN header. Its
read buffer is sized to exactly that contract rather than adding truncation
detection or recovery for frames the kernel must not produce; correctness
depends on that size guarantee. A missing vnet header, invalid checksum or GSO
metadata, or an unexpected offload is an internal kernel or configuration
violation and fails fast rather than being handled as a recoverable packet
error. The Ethernet frame contents remain untrusted input and are forwarded to
the guest without protocol parsing.

TAP offloads are reset when the endpoint attaches because a persistent device
can retain state from an earlier user. Packets already queued before that reset
can still carry any inherited checksum or GSO metadata, including ECN or UDP
segmentation enabled by the earlier user, so the first queue drops such packets
until the retained queue is observed to drain. The TAP file descriptor also
persists across queue recreation. After a capability downgrade, TAP temporarily
accepts only the capabilities removed from that configuration. It consumes and
drops a matching stale packet without completing a frontend buffer, then retries
the oldest buffer with the next frame. Unfinished recovery is preserved across
another queue recreation. The recovery window closes only after a successful
read is followed by a read that reaches `EAGAIN`; an initial asynchronous
`Pending` does not prove that the queue drained. Steady-state and other vnet
metadata violations still fail fast.

Virtio-net receive GSO is disabled by default. Advertising guest TCP
segmentation offload without mergeable receive buffers causes Linux guests to
allocate receive buffers large enough for a complete coalesced packet, commonly
64 KiB per descriptor. OpenVMM can opt in with the `rx_gso:` prefix on a
`--virtio-net` argument when testing or when that allocation tradeoff is
acceptable.

When enabled, receive GSO is advertised only when the selected backend reports
that it can produce coalesced receive packets. TCP GSO also requires guest
checksum support; a guest that selects TCP GSO without checksum support does not
cause the backend to request an invalid TAP offload configuration. Receive GSO
can become the default after virtio-net supports distributing one packet across
multiple mergeable receive buffers.

Frontends must also publish a receive completion only after the packet
payload and frontend-specific header have both been written successfully.
For virtio-net, a capacity or guest-memory write failure leaves the
descriptor as a dropped receive and increments an explicit drop counter;
it is never counted as a successfully received packet.

Frontends reject intrinsically unusable receive buffers before posting them
to a backend. For example, virtio-net completes a descriptor that cannot hold
a minimum Ethernet header as a zero-length dropped receive. If a valid TAP
frame is larger than the oldest posted buffer, TAP drops that frame but retains
the buffer at the front of the queue for the next frame. It does not skip ahead
to a later buffer, because frontend completions may need to be published in the
original descriptor order.

## Lifecycle

1. The frontend creates a
   [`BufferAccess`](https://openvmm.dev/rustdoc/net_backend/trait.BufferAccess.html)
   implementation and one `QueueConfig` per queue.
2. It calls `endpoint.get_queues(configs, rss, &mut queues)`.
3. It enters the poll loop: `poll_ready` → `rx_avail` / `rx_poll` /
   `tx_avail` / `tx_poll`.
4. On shutdown, it drops the queues and calls `endpoint.stop()`.

## Backends

| Backend | Crate | Transport | Platform |
|---------|-------|-----------|----------|
| TAP | `net_tap` | Linux TAP device | Linux |
| DirectIO | `net_dio` | Windows vmswitch | Windows |
| Consomme | `net_consomme` | User-space TCP/IP stack | Any |
| MANA | `net_mana` | Azure hardware NIC (MANA/GDMA) | Linux |
| Loopback | `net_backend` | Reflects TX → RX | Any |
| Null | `net_backend` | Drops everything | Any |

## Frontends

| Frontend | Crate | Guest interface |
|----------|-------|-----------------|
| virtio-net | `virtio_net` | Virtio network device |
| netvsp | `netvsp` | VMBus synthetic NIC |
| GDMA/BNIC | `gdma` | MANA Basic NIC (emulated GDMA) |

## Wrappers

Wrappers implement `Endpoint` by delegating to an inner endpoint,
adding cross-cutting behavior:

- **PacketCapture** (`net_packet_capture`) — intercepts `rx_poll`
  and `tx_avail` to write PCAP-format packet traces. The capture
  path reads packet data from guest memory via `BufferAccess` and
  writes enhanced packet blocks to a ring buffer. Capture can be
  toggled at runtime; when disabled, the wrapper adds only an atomic
  load per call.

- **Disconnectable** (`net_backend`) — supports hot-plug and
  hot-unplug by swapping the inner endpoint at runtime.

## RSS and multi-queue

When a frontend supports Receive Side Scaling (RSS), it passes
multiple `QueueConfig` entries and an `RssConfig` (hash key +
indirection table) to `get_queues`. The backend creates one `Queue`
per entry and uses the RSS configuration to steer incoming packets
to the appropriate queue. Each queue is driven independently by its
own async task.

Currently `netvsp` and `net_mana` support multi-queue; `virtio_net`
is limited to a single queue pair.
