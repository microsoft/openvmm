# VNC Server Architecture

## Overview

The openvmm VNC server provides remote graphical console access to virtual
machines. It supports multiple concurrent clients on a single port, device-
driven dirty region tracking for efficient updates, and raw, zlib, and ZRLE
framebuffer encoding.

```text
                                    +-----------------+
                                    | Guest VM        |
                                    | (writes VRAM)   |
                                    +--------+--------+
                                             |
              Synthetic Video Device         | VRAM (shared memory, 8MB)
              (vm/devices/uidevices)         |
                    |                        |
                    | DirtyRect channel      |
                    v                        v
  +------------------------------------------+------------------+
  | VNC Worker (vnc_worker/src/lib.rs)                          |
  |                                                             |
  |  MultiClientServer                                          |
  |  +------ accept() -----> spawn_client() ---+                |
  |  |                                         |                |
  |  +------ dirty_recv --> broadcast ----+    |                |
  |  |                                    |    |                |
  |  |   +--- Client 1 (vnc::Server) <---+     |                |
  |  |   |   - own zlib stream                 |                |
  |  |   |   - own pixel format                |                |
  |  |   |   - own UpdateState                 |                |
  |  |   |   - own Encoder                     |                |
  |  |   |                                     |                |
  |  |   +--- Client 2 (vnc::Server) <--------+                 |
  |  |   |   ...                                                |
  |  |   +--- Client N (up to --vnc-max-clients)                 |
  |  |                                                          |
  |  +-- SharedView (Arc<Mutex<ViewWrapper>>)  <-- read VRAM    |
  |  +-- SharedInput (mesh::Sender<InputData>) --> keyboard/mouse|
  +-------------------------------------------------------------+
```

## Crate Structure

### `vnc` crate (`workers/vnc_worker/vnc/`)

The protocol implementation. Handles a single RFB connection.

| Module            | Purpose                                                                             |
|-------------------|-------------------------------------------------------------------------------------|
| `lib.rs`          | `Server<F,I>`, `Encoder`, `UpdateState`, `ClientState`, handshake, message dispatch |
| `dirty_bitmap.rs` | `DirtyBitmap` with configurable tile tracking, `Rect` type                          |
| `rfb.rs`          | Wire format structs (PixelFormat, Rectangle, etc.) and protocol constants           |
| `scancode.rs`     | X11 keysym to US scancode conversion tables                                         |

### `vnc_worker` crate (`workers/vnc_worker/`)

The multi-client orchestrator. Manages connections and broadcasts dirty rects.

| Type                   | Purpose                                                   |
|------------------------|-----------------------------------------------------------|
| `VncWorker<T>`         | Mesh worker entry point, constructs `MultiClientServer`   |
| `MultiClientServer<T>` | Accept loop, dirty rect broadcast, client lifecycle       |
| `SharedView`           | `Arc<Mutex<ViewWrapper>>` implementing `vnc::Framebuffer` |
| `SharedInput`          | `mesh::Sender<InputData>` implementing `vnc::Input`       |

### `vnc_worker_defs` crate

`VncParameters<T>` is the mesh-serializable config passed from `openvmm_entry`
to the worker. Contains listener, framebuffer access, input sender, and
`synth_video: Option<SynthVideoChannels>`, which bundles the dirty-rect
receiver and the "updates needed" sender (the latter tells the video device
when a client is connected). The two are present together or not at all, so the
mismatched state cannot be constructed. The device end mirrors this with
`SynthVideoHandle.channels: Option<SynthVideoDeviceChannels>`.

### `video_core` crate

`DirtyRect` is the shared type for dirty rectangles between the synthetic video
device and the VNC worker. Defined here to avoid circular dependencies.

## Data Flow: Framebuffer Update Cycle

Every 30ms, a per-client timer fires and triggers an update check:

```text
Timer fires (30ms)
    |
    v
Updater.update() --> mpsc channel --> Server event loop wakes
    |
    v
collect_dirty()
    |
    +-- Drain device dirty channel (Arc<Vec<DirtyRect>> from coordinator)
    |   If rects received: mark tiles in pending_dirty bitmap
    |
    +-- Choose update mode:
    |   (a) cur_fb stale (first frame, resolution change, dropped broadcast):
    |       read entire VRAM, mark all tiles dirty
    |   (b) device dirty rects, or a client full refresh while the device keeps
    |       cur_fb current: O(1) swap prev_fb/cur_fb, read only the dirty-rect
    |       pixels. A full refresh here sends the whole up-to-date buffer
    |       without re-reading VRAM
    |   (c) device_dirty_seen but empty channel: nothing changed, skip entirely
    |   (d) full refresh with no device buffer: read entire VRAM, mark all tiles
    |   (e) no device support: read entire VRAM, tile-diff against prev_fb
    |
    +-- merge_dirty_rects(): bitmap --> merged Rect list
    |   Pass 1: merge horizontally adjacent tiles into row spans
    |   Pass 2: merge vertically adjacent spans with same x and width
    |
    v
Encode dirty rects into output_buf (single buffer, one socket write)
    |
    +-- For each Rect:
    |   Encoder.encode_rect():
    |     - Convert pixels (PixelConversion: cached shifts, no-convert fast path)
    |     - Compress with zlib (continuous stream per connection) or send raw
    |     - Append rect header + data to output_buf
    |
    v
socket.write_all(&output_buf)  -- single TCP write for entire update
    |
    v
update_state.commit()  -- swap cur_fb into prev_fb for next cycle
```

## Dirty Region Tracking

### The Problem

The guest writes to VRAM (8MB shared memory) at any time. The VNC server
needs to know which regions changed to avoid sending the entire framebuffer
every 30ms.

### Three Dirty Sources

**1. Device dirty rects (modern guests)**

The synthetic video device (Hyper-V SYNTHVID protocol) receives `DirtMessage`
from the guest video driver with pixel-coordinate rectangles of changed
regions. These are forwarded via a `mesh::channel` from the video device's
dirt sender (`SynthVideoHandle.channels`) to the VNC worker's dirty-rect
receiver (`VncParameters.synth_video`).

Supported drivers:
- Windows: full support (DWM reports dirty regions)
- Linux `hyperv_drm`: full support (sends `SYNTHVID_DIRT` messages)
- Linux `hyperv_fb` (older): does NOT send dirty rects

When device dirty rects arrive, the server reads only those rectangles from
VRAM (the dirty columns of the dirty rows), not whole scanlines. An idle 1080p
desktop reads 0 bytes per cycle instead of 8MB.

**2. Tile diff fallback (old guests)**

When no device dirty rects are available (`hyperv_fb`), the server reads the
entire framebuffer and compares it tile-by-tile against the previous frame.

- Tile size: configurable via `--vnc-tile-size` (4, 8, or 16; default 8x8); see
  [Dirty-tracking tile size](tile-size.md)
- Comparison uses `==` on `&[u32]` slices (delegates to `memcmp`, SIMD-optimized)
- Changed tiles are marked in `pending_dirty` via `set_tile()` (direct bit set)

**3. Full refresh**

Triggered on: first frame, resolution change, client pixel format change,
non-incremental `FramebufferUpdateRequest`. When the synth device is keeping
cur_fb current, a client-requested full refresh sends that buffer as-is rather
than re-reading VRAM. Only a stale buffer (first frame, resolution change,
dropped broadcast) forces a fresh full read.

### DirtyBitmap

A packed `Vec<u64>` with one bit per tile (8x8 by default, configurable via
`--vnc-tile-size`). At 1920x1080 with 16x16 tiles: 120x68 = 8160 tiles, ~128
u64 words, ~1KB.

Key operations:
- `mark_rect(l, t, r, b)`: convert pixel coords to tile bits, set in bulk
  (per-word for wide rects, not per-bit)
- `merge_into(out)`: two-pass merge (horizontal then vertical) produces a
  minimal rectangle list for encoding
- `set_tile(tx, ty)`: direct bit-set by tile coords (used by tile_diff)
- `or_from(other)`: accumulate from another bitmap (for multi-client)

### Idle Desktop Optimization

Once device dirty rects have been received at least once (`device_dirty_seen`),
subsequent empty cycles skip the VRAM read entirely. This reduces an idle
1080p desktop from 8MB/frame/client to 0 bytes.

### Suppressing Guest Reporting When No Client Is Connected

The optimization above still lets the guest generate and send dirty
rectangles even when nobody is connected. To avoid that, the VNC worker
tells the synthetic video device whether any client is connected: a `bool`
sent over a `mesh` channel (paired with the dirty-rect channel), `true` on
the first connect and `false` when the last client leaves. When updates are
not needed, the device sends a synthvid `FeatureChange` with `is_dirt_needed`
and the pointer flags cleared, so a cooperating guest stops reporting
altogether. It re-enables on the next connect.

Situation (resolution) updates stay enabled the whole time, so a mode change
while idle is still tracked and the next client renders at the right size.
The device also drops any dirt that arrives while reporting is disabled, and
logs a rate-limited warning if a guest keeps sending it, which indicates the
guest is not honoring the request.

The device side defaults to the guest's enabled state and only disables when
the coordinator signals "no client". This keeps a connected client working
across a guest reboot or video-driver reload: those re-open the synthvid
channel and reset the device's per-connection state, but the coordinator does
not re-signal (the client never left), so the guest keeps reporting. The flip
side is that a reopen while no client is connected leaves the guest reporting
to nobody. The coordinator corrects this on its own: if it receives device
dirt while it has zero clients, it re-sends "no client", which settles in one
cycle (the device then drops further dirt at the gate).

## Multi-Client Architecture

### Connection Lifecycle

```text
TcpListener.accept()
    |
    v
Check --vnc-max-clients limit (default 16)
    |
    +-- --vnc-evict-oldest: disconnect oldest client to make room
    +-- default: reject with TCP RST if exceeded
    |
    v
Set TCP_NODELAY -- disable Nagle's algorithm for latency
    |
    v
spawn_client():
    - Clone SharedView (Arc)
    - Clone SharedInput (mesh::Sender)
    - Create per-client dirty mpsc channel (capacity 4)
    - Create vnc::Server with own zlib stream, pixel format, UpdateState
    - Spawn as async future in FuturesUnordered
    - 30ms PolledTimer drives Updater channel
```

### Eviction and Client Counting

The active client count uses `abort_senders.len()`, not `clients.len()`.
When a client is evicted, its abort sender is removed immediately, but
its async future stays in `clients` until the next poll reaps it via the
`ClientDone` event. This means `clients.len()` can transiently exceed
`max_clients` during rapid connection churn (e.g., three connects in
quick succession with `max_clients=1`). This is intentional: the dying
futures are in their abort cleanup path and resolve within one poll
cycle. Awaiting the evicted client before spawning the replacement would
block the accept loop and add latency to every new connection.

### Dirty Rect Broadcast

The coordinator receives device dirty rects on its `synth_video` channel and
broadcasts them to all clients via per-client `mpsc::Sender<Arc<Vec<DirtyRect>>>`.
It drains the whole channel backlog on each wakeup and tracks the high-water
mark. If the unbounded mesh queue ever climbs past `MAX_DIRTY_BACKLOG`, which
means a wedged consumer, it logs a rate-limited error.

The drained backlog is coalesced into a single broadcast: a backlog of N
messages costs one send, not N. Concatenating is lossless because each client
merges rects into its own bitmap before encoding. Broadcasting per-message
instead overflows the small bounded per-client channel on a backlog burst, which
forces a full-screen retransmit every cycle. A coalesced set larger than
`MAX_COALESCED_DIRTY_RECTS` is dropped in favor of one full refresh (bounded by
the screen size).

- `Arc` wrapping avoids cloning the rect Vec per client (ref-count bump only)
- If a client's channel is full (slow consumer), `missed_dirty` is set with a
  debug log, and the client does one full refresh next cycle to resync
- Each client drains its channel independently during `collect_dirty()`

### Per-Client Isolation

Each client has independent:
- Pixel format (8/16/32 bpp, endianness, color channel layout)
- Zlib compression stream (RFB requires continuous stream per connection)
- Framebuffer snapshots (cur_fb, prev_fb) for tile diff
- Encoding capabilities (zlib, cursor, desktop resize, QEMU extended keys)
- Update state (force_full, ready_for_update, device_dirty_seen)

Shared across clients:
- Framebuffer VRAM (read-only via `SharedView`, `Mutex` for channel polling)
- Input sender (all clients' keyboard/mouse goes to the same VM)

### Per-Client Logging

Each client's task runs inside a tracing span tagged with its id, so log lines
from the worker and the `vnc::Server` it drives (encoding negotiation, updates
sent, full-screen retransmits) are attributable to a specific connection when
several are active. The span renders as `vnc_client{id=N}` ahead of the target
and message.

### Worker Restart

On `WorkerRpc::Restart`:
1. Drop all abort senders (closes oneshot channels)
2. Clear dirty senders
3. Drive all client futures to completion
4. `Arc::try_unwrap` the shared view (all clients terminated)
5. Return `VncParameters` to the mesh framework

## Pixel Format Conversion

### PixelConversion

Pre-computed per-connection, cached in `ClientState.pixel_conv`. Avoids
recomputing `leading_zeros()` + shift arithmetic per pixel.

```rust,ignore
struct PixelConversion {
    dest_depth: usize,     // bytes per pixel (1, 2, or 4)
    shift_r/g/b: u32,      // right-shift to align channel to client bit width
    out_shift_r/g/b: u8,   // left-shift to place at client's bit position
    big_endian: bool,
    no_convert: bool,       // true when format matches internal 0x00RRGGBB
}
```

Bit widths derived from `leading_zeros()` on the format's max values, not
`count_ones()` (which gives wrong results for non-conforming values).

### Fast Path

When `no_convert` is true (client uses native 32bpp RGBA, the common case),
pixel data is copied directly via `extend_from_slice(src.as_bytes())`,
with no per-pixel computation.

The no-convert check is hoisted out of the per-scanline encode loop to avoid
a function call + branch per scanline.

## Encoding Pipeline

### Batched Output

The entire `FramebufferUpdate` message (header + cursor + all rect data) is
accumulated in `Encoder.output_buf` and sent with a single
`socket.write_all()`. This reduces async write calls from O(rects * 3) to
O(1) per update cycle.

### Zlib Compression

Per-connection `flate2::Compress` stream using `Compression::fast()`.
The stream is continuous across all rects in all updates (RFB requirement).
`FlushCompress::Sync` is used to emit output while preserving the dictionary.

Buffer management: initial allocation of `input_size + 128` bytes. The Vec
retains capacity across calls, so after the first large rect it's a no-op.
Doubling fallback for rare incompressible data.

## Keyboard Input

See [keyboard.md](keyboard.md) for the full keyboard handling documentation,
including the Ctrl+Alt+P clipboard paste mechanism, Alt+Numpad input for
non-ASCII characters, and client compatibility notes.

## Performance Characteristics

| Scenario                   | VRAM reads    | CPU work                       |
|----------------------------|---------------|--------------------------------|
| Idle desktop, device dirty | 0 bytes/cycle | Channel drain only             |
| Idle desktop, tile diff    | 8MB/cycle     | memcmp 8160 tiles              |
| Small update, device dirty | ~1KB/cycle    | Read dirty columns + encode    |
| Full refresh, device buffer| 0 bytes/cycle | Encode cached frame, no re-read |
| Full screen, zlib          | 8MB/cycle     | Full read + zlib compress      |
| Full screen, raw           | 8MB/cycle     | Full read + memcpy             |

With TCP_NODELAY, per-update latency is dominated by zlib compression
and TCP transmission, not server-side processing.
