# StorVSP Channels & Subchannels

StorVSP uses VMBus [channels and subchannels](../../architecture/vmbus/channels.md) to
parallelize SCSI I/O across guest VPs. This page covers how StorVSP
negotiates subchannels, how the worker model maps to channels, and
the performance characteristics that matter in practice.

For the general VMBus channel model, see [VMBus Channels](../../architecture/vmbus/channels.md).
For the storage pipeline architecture, see
[Storage Pipeline](../../architecture/devices/storage.md).

## Subchannel negotiation

StorVSP and the guest `storvsc` driver negotiate subchannels through
the StorVSP protocol — a Hyper-V-specific wire format carried over
VMBus (for the protocol definition, see the
[`storvsp_protocol` rustdoc](https://openvmm.dev/rustdoc/linux/storvsp_protocol/index.html)
and the [StorVSP](storvsp.md) device page). The VMBus protocol handles
the underlying channel offers; the StorVSP protocol controls *how many*
subchannels are created.

```mermaid
sequenceDiagram
    participant G as Guest (storvsc)
    participant S as StorVSP
    participant V as VMBus framework

    Note over G,S: StorVSP protocol messages
    G->>S: BEGIN_INITIALIZATION
    S-->>G: version ack
    G->>S: QUERY_PROTOCOL_VERSION
    Note right of G: Guest proposes a version
    S-->>G: version reply (accept/reject)
    G->>S: QUERY_PROPERTIES
    S-->>G: ChannelProperties
    Note right of S: Includes multi_channel_flag<br/>and maximum_sub_channel_count

    Note over G: Guest decides how many<br/>subchannels to request<br/>(up to the advertised max)
    G->>S: CREATE_SUB_CHANNELS(n)

    Note over S,V: VMBus protocol messages
    S->>V: ChannelControl::enable_subchannels(n)
    V-->>G: n × OfferChannel<br/>(subchannel_index 1..n)
    Note over G: Guest opens each subchannel<br/>(chooses target_vp per channel)

    Note over G,S: StorVSP protocol messages
    G->>S: END_INITIALIZATION
    Note over G,S: Ready — I/O flows on any open channel
```

Key points:

- The **host** advertises the maximum subchannel count via
  `ChannelProperties`. The **guest** decides how many to create (up to
  that maximum).
- `CREATE_SUB_CHANNELS` is a StorVSP protocol message. The resulting
  channel offers are VMBus protocol messages.
- The guest can also send `CREATE_SUB_CHANNELS` after initialization
  (in the Ready state).
- Multi-channel support requires StorVSP protocol version Win8 or later.
  Guest driver support:
  - **Linux:** `hv_storvsc` multi-channel support was added in kernel
    3.10 (2013).
  - **FreeBSD:** `hv_storvsc` multi-channel support landed in
    FreeBSD 12.2.
  - **Windows:** supported since Windows Server 2012 (the first
    release with the Win8 protocol version).

## Worker model

StorVSP runs **one async worker per open channel**. That means one
worker for the primary channel, plus one worker for each open
subchannel — so a device with 3 subchannels has 4 workers total.
Each worker owns:

- A `Queue` wrapping the channel's ring buffer pair.
- A `FuturesUnordered` for concurrent SCSI request processing.
- Its own `max_io_queue_depth` limit.
- Per-worker statistics and inspect output.

```text
  StorVSP device instance
  ┌──────────────────────────────────────────────────┐
  │  Worker 0 (primary channel)                      │
  │    ├─ Protocol negotiation (init only)           │
  │    ├─ Queue (incoming + outgoing ring)           │
  │    └─ FuturesUnordered<ScsiRequest>              │
  ├──────────────────────────────────────────────────┤
  │  Worker 1 (subchannel 1)                         │
  │    ├─ Queue (own ring pair)                      │
  │    └─ FuturesUnordered<ScsiRequest>              │
  ├──────────────────────────────────────────────────┤
  │  Worker 2 (subchannel 2)                         │
  │    └─ ...                                        │
  └──────────────────────────────────────────────────┘
  All workers share: ScsiController (disk map), Protocol state
```

The **primary channel** handles protocol negotiation; subchannels wait
for the protocol to reach the Ready state before processing I/O.
If a guest sends I/O on a subchannel before protocol initialization
completes, the subchannel worker blocks (via an async listener) until
the primary channel finishes negotiation — the I/O is not lost, just
delayed. After initialization, all channels process I/O identically.

### I/O dispatch

There is no host-side steering. The guest chooses which channel to
write each I/O request into — typically matching the current VP to
the subchannel opened with that VP as the target. Each worker reads
only its own ring.

Outstanding I/O is capped **per channel** (per worker), not per
controller or per disk. Each worker has its own `max_io_queue_depth`
— when that many SCSI requests are in flight on a single channel,
the worker stops reading new requests from the ring until one
completes. Requests on other channels are unaffected.

## CPU affinity

Subchannels exist so the guest can place I/O close to the VP that
initiated it. This is the most important "why" behind subchannels —
not just parallelism, but *locality*.

### How targeting works

When a channel is opened, StorVSP creates the worker's async driver
targeted at the VP the guest specified:

```rust
// storvsp new_worker() — simplified
let driver = self.driver_source
    .builder()
    .target_vp(target_vp)
    .run_on_target(true)
    .build("storvsp-worker");
```

The strength of this targeting depends on the executor backend:

| Backend | `run_on_target` behavior |
|---------|-------------------------|
| OpenHCL threadpool | Strong — CPU-affinitized thread, tasks run on the target CPU. In-flight I/Os are not retargeted. |
| OpenVMM thread backend | Weaker — dedicated worker thread, but no physical CPU affinity. `retarget_vp()` is a no-op. |
| Single-driver backend | Ignored — everything runs on one thread. |

```admonish note title="VP index = CPU number (today)"
The `target_vp` in the VMBus open request is a hypervisor VP index.
In OpenHCL, VP index is used directly as the Linux CPU number for
threadpool targeting. This is a simplifying assumption — see the
[VP index, CPU number, and APIC ID](../../architecture/vmbus/channels.md#vp-index-cpu-number-and-apic-id)
section in the VMBus Channels page for details.
```

### What happens during a read

```mermaid
sequenceDiagram
    participant App as Guest App (VP 2)
    participant SV as storvsc driver
    participant Ring as VMBus Ring
    participant W as StorVSP Worker
    participant Disk as AsyncScsiDisk

    App->>SV: read() syscall
    SV->>Ring: write SCSI READ(16) packet
    SV->>W: signal host (interrupt → VP 2)
    Note over App: Guest VP not stalled —<br/>can execute other work
    W->>Ring: poll_read_batch()
    Ring-->>W: SCSI request packet
    W->>Disk: execute_scsi().await
    Note over W: cooperative yield point
    Disk-->>W: completion (data + status)
    W->>Ring: write completion packet
    W->>SV: signal guest (interrupt → VP 2)
    SV-->>App: read data available
```

The guest VP is **not stalled** during this process. Writing to the
ring and signaling the host are non-blocking operations in guest
memory and hypercall space — the VP can continue executing guest code
immediately after the signal. The guest kernel only blocks the
*application thread* when it does a synchronous `read()` syscall (the
kernel puts that thread to sleep until the completion interrupt
arrives). Other threads and other VPs continue running normally. The
VP itself is never taken out of VTL0 by the I/O submission.

### Retargeting

The guest can retarget a channel to a different VP via `ModifyChannel`.
This happens when VPs come online/offline (e.g., CPU hot-remove) and
the guest rebalances channel assignments. StorVSP forwards the
retarget to its worker's driver — future work moves to the new VP's
thread, but in-flight I/Os complete on the old one.

### IDE accelerator comparison

The IDE accelerator uses StorVSP to back an IDE device via a VMBus
channel, replacing the slow PCI port-I/O emulation path with VMBus
ring buffers. However, the IDE accelerator sets
`max_sub_channel_count = 0` — all I/O is serialized through a single
channel. The accelerator exists for throughput (ring buffers are
faster than port-I/O VM exits), not for parallelism.

### Frontend comparison

| Frontend | VP affinity model |
|----------|-------------------|
| **StorVSP** (VMBus SCSI) | Guest chooses VP per channel. Subchannels = explicit multi-queue. One worker per channel, VP-targeted. |
| **NVMe** (emulated) | Guest creates submission queues mapped to VPs via MSI-X vectors. One async handler per completion queue. No VMBus — MMIO doorbells. |
| **IDE** (emulated) | Single channel, no subchannels. All I/O serialized through one path. |
| **IDE accelerator** | StorVSP-backed, but `max_sub_channel_count = 0`. Single channel. |

## Subchannel scaling

The number of subchannels determines how much parallelism is
available. Too few and I/O contends on shared rings; too many and
resources are wasted.

### 0 subchannels (default)

All I/O funnels through the primary channel — one ring, one worker.

```text
  Guest VPs          Channels              StorVSP Workers
  ┌──────┐
  │ VP 0 ├───┐       ┌─────────────┐       ┌──────────────┐
  └──────┘   ├──────►│ Primary (0) ├──────►│ Worker 0     │
  ┌──────┐   │       │  ring pair  │       │ all I/O      │
  │ VP 1 ├───┘       └─────────────┘       └──────────────┘
  └──────┘
```

On a 64-VP VM, this means 64 VPs compete for one ring. The single
worker's `max_io_queue_depth` limits total concurrency.

### One channel per VP (ideal)

Each VP gets its own channel. No ring contention, no cross-VP
serialization.

```text
  ┌──────┐           ┌─────────────┐       ┌──────────────┐
  │ VP 0 ├──────────►│ Primary (0) ├──────►│ Worker 0     │
  └──────┘           └─────────────┘       └──────────────┘
  ┌──────┐           ┌─────────────┐       ┌──────────────┐
  │ VP 1 ├──────────►│ Subchan (1) ├──────►│ Worker 1     │
  └──────┘           └─────────────┘       └──────────────┘
       ...                 ...                   ...
  ┌──────┐           ┌─────────────┐       ┌──────────────┐
  │ VP N ├──────────►│ Subchan (N) ├──────►│ Worker N     │
  └──────┘           └─────────────┘       └──────────────┘
```

Each channel costs a ring buffer GPADL and a worker task.

### Over-provisioned (subchannels > VPs)

The guest *could* open more channels than it has VPs — the protocol
doesn't prevent it. But there's no benefit: each channel needs a
target VP, and mapping multiple channels to the same VP just
recreates the contention problem on that VP's ring. In practice,
guest drivers open at most one channel per VP. Extra subchannel
offers go unused.

### VP-to-channel assignment

The mapping of VPs to channels is entirely up to the guest driver.
The StorVSP protocol does not prescribe any assignment.

- **Linux** `storvsc` typically opens one subchannel per online VP
  and steers I/O by matching the current CPU to the corresponding
  channel (controlled by `storvsc_vcpus_per_sub_channel`).
- **Windows** `storvsc` also opens subchannels and distributes I/O
  across them, though the exact allocation strategy is not public.
- **FreeBSD** `hv_storvsc` supports multi-channel since FreeBSD 12.2
  with similar per-CPU steering.

## Performance characteristics

### Poll mode

StorVSP has two ring-reading modes, controlled by
`poll_mode_queue_depth` (default: 1). This value can be changed at
runtime via the inspect tree — it's stored as an `AtomicU32` and
exposed through `inspect::AtomicMut`:

```bash
# View current poll mode queue depth
openvmm inspect storvsp/poll_mode_queue_depth

# Change it at runtime (e.g., to 8)
openvmm inspect storvsp/poll_mode_queue_depth --update 8
```

```mermaid
stateDiagram-v2
    [*] --> InterruptDriven: worker starts

    InterruptDriven --> PollMode: pending_io >= poll_mode_queue_depth
    PollMode --> InterruptDriven: pending_io < poll_mode_queue_depth

    InterruptDriven: Interrupt-driven mode
    InterruptDriven: poll_read_batch(cx) — registers waker
    InterruptDriven: Sleeps until guest signals
    InterruptDriven: Low CPU cost, higher latency

    PollMode: Busy-poll mode
    PollMode: try_read_batch() — no waker
    PollMode: Spins checking ring
    PollMode: High CPU cost, lower latency
```

With `poll_mode_queue_depth = 1`, even a single in-flight I/O
triggers poll mode. This benefits fast backends (NVMe-backed storage)
where the interrupt round-trip cost is significant relative to I/O
latency. For higher-latency remote disks, poll mode wastes CPU —
that's why OpenHCL sets `poll_mode_queue_depth = 4` on the
remote-disk SCSI controller.

### The slow-disk problem

All LUNs on a SCSI controller share the same channel pool.
Subchannels parallelize across VPs, but they don't isolate LUNs.

```text
  Channel 0 (Worker 0)
  ┌──────────────────────────────────────────────────┐
  │  FuturesUnordered (max_io_queue_depth = 256)     │
  │                                                  │
  │  ┌────────┐ ┌────────┐ ┌────────┐ ┌────────┐   │
  │  │ LUN 0  │ │ LUN 0  │ │ LUN 1  │ │ LUN 0  │   │
  │  │  Read  │ │ Write  │ │  Read  │ │  Read  │   │
  │  │ (fast) │ │ (fast) │ │ (slow) │ │ (fast) │   │
  │  └────────┘ └────────┘ └────────┘ └────────┘   │
  └──────────────────────────────────────────────────┘
```

If a slow LUN fills the queue depth with long-running I/Os, fast LUN
I/Os on the same channel are blocked behind them. The only mitigation
is controller separation: put fast and slow disks on different SCSI
controllers.

### Cooperative executor (OpenHCL)

In OpenHCL, each VP runs on a single thread with a cooperative
(non-preemptive) async executor. All tasks targeted at VP N share
that thread — storvsp workers, other device workers, and the VP
dispatch loop (VTL0 entry/exit). This means VP stalls (VTL0
residence, kernel syscall blocking, hypervisor intercepts) directly
impact storvsp worker latency.

Subchannels help with *channel contention* but not with *VP stalls*.
If a VP spends most of its time in VTL0, its subchannel is
effectively idle. Spreading subchannels across VPs mitigates this.

On x64 non-isolated VMs, the [sidecar kernel](../../architecture/openhcl/sidecar.md)
changes the picture: guest VTL0 execution runs on sidecar CPUs while
device workers run on Linux CPUs. Initially, device work is handled
by the few Linux CPUs that booted (typically one per NUMA node). As
sidecar VPs are onlined into Linux — triggered by the first VM exit
requiring VTL2 processing — the load spreads to more CPUs.

```admonish tip
For the full executor model, blocking scenarios, mitigations, and
sidecar tradeoffs, see the
[CPU Scheduling](../../architecture/openhcl/cpu_scheduling.md) page.
```

## Configuration

### OpenVMM CLI

```bash
# Default: no subchannels (all I/O on primary channel)
openvmm --disk memdiff:file:disk.vhd

# 4 subchannels (5 total channels: 1 primary + 4 sub)
openvmm --scsi-sub-channels 4 --disk memdiff:file:disk.vhd
```

The enforced maximum is 1023 (one less than `MAX_PROCESSOR_COUNT`).
In practice, there's no benefit to more subchannels than VPs — at
most one channel per VP is useful.

The count applies to all SCSI controllers in the VM — it's a global
setting, not per-controller.

### OpenHCL (VTL2 settings)

The `scsi_sub_channels` field in the fixed VTL2 settings controls the
maximum subchannel count. The runtime value is clamped to
`min(configured, 256)`.

```admonish example title="VTL2 settings — enabling subchannels"
~~~json
{
  "version": 1,
  "fixed": {
    "scsi_sub_channels": 16,
    "io_ring_size": 256
  },
  "dynamic": {
    "scsi_controllers": [ "..." ]
  }
}
~~~
```

| Field | Default | Meaning |
|-------|---------|---------|
| `scsi_sub_channels` | 0 | Maximum subchannel count for all SCSI controllers. Clamped to 256. |
| `io_ring_size` | 256 | Size of each per-CPU io_uring submission queue in the OpenHCL threadpool. Controls how many async I/O operations (disk reads, writes, network, etc.) can be submitted to the kernel at once per CPU thread. This is **not** the StorVSP per-channel queue depth — that is configured per controller via `io_queue_depth` in the dynamic SCSI controller settings. |

StorVSP's per-channel queue depth (`max_io_queue_depth`) is set from
`ScsiControllerHandle.io_queue_depth`, which defaults to the
`default_io_queue_depth` passed at controller creation. This is
separate from `io_ring_size`.

### Guest-side kernel params

OpenHCL's boot shim sets these Linux kernel parameters for the VTL0
guest's `storvsc` driver:

```text
hv_storvsc.storvsc_vcpus_per_sub_channel=2048
hv_storvsc.storvsc_max_hw_queues=2
hv_storvsc.storvsc_ringbuffer_size=0x8000
```

These affect the guest driver's subchannel creation behavior,
independent of the host-side maximum.
`storvsc_vcpus_per_sub_channel=2048` means the driver creates one
subchannel per 2048 VPs — effectively suppressing subchannels for
typical VM sizes.

## Hyper-V differences

Hyper-V's VMBus server uses the same wire protocol as OpenVMM (the
same `OfferChannel`, `OpenChannel`, `CREATE_SUB_CHANNELS` messages),
so guest drivers work identically against both. However, the
host-side scheduling and targeting behavior differs:

- Hyper-V uses proprietary algorithms for interrupt delivery,
  channel scheduling, and VP targeting that are not open source.
- OpenVMM's implementation is simpler: one async worker per channel,
  guest-controlled VP targeting, no host-side cross-channel balancing.

```admonish warning
When comparing performance between OpenVMM and Hyper-V, focus on the
wire protocol (which is shared) rather than assuming host-side
behavior matches. Differences in throughput or latency are typically
due to host-side scheduling, not protocol incompatibility.
```

## Inspect

StorVSP exposes channel state through the inspect tree:

```text
storvsp/
  channels/
    0/                          # primary channel
      state: ready
      version: Win8
      subchannel_count: 2
      pending_packets: 0
      stats: { ... }
      ring: { ... }
      max_io_queue_depth: 256
    1/                          # subchannel 1
      pending_packets: 3
      io/
        0/
          transaction_id: 42
          address: 0:0:0
          operation: ScsiCommand(Read16)
      stats: { ... }
      ring: { ... }
      max_io_queue_depth: 256
  disks/
    0:0:0/
      disk: { ... }
  poll_mode_queue_depth: 1
```

The primary channel (index 0) shows the negotiated protocol version
and subchannel count. All channels show pending packets, per-I/O
state, and ring buffer statistics.

At the VMBus level, subchannels appear under:
`vmbus/<instance_id>/subchannels/<n>` — showing offer state, channel
ID, and connection state.
