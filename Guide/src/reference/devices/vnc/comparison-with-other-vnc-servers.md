# Comparison with other VNC servers

A feature comparison of the OpenVMM VNC server with QEMU, TigerVNC, and
libvncserver: what it does differently, and what it still lacks.

## Where this implementation differs

### Per-Client Isolation in Multi-Client Mode

QEMU, TigerVNC, and libvncserver all support multiple concurrent clients.
We do too (up to `--vnc-max-clients`, default 16). What distinguishes our
implementation is the degree of per-client isolation:

- Independent pixel format per client (8bpp and 32bpp viewers simultaneously)
- Separate zlib compression stream per client (RFB requires this, but not
  all implementations get it right under concurrency)
- Independent framebuffer snapshots and dirty tracking per client
- Independent encoding negotiation (one client can use ZRLE, another raw)
- Optional oldest-client eviction (`--vnc-evict-oldest`) for admin takeover

Each client is a fully independent `vnc::Server` instance sharing only
the read-only framebuffer and input channel.

### Device-Driven Dirty Tracking

All VNC servers need to know which parts of the screen changed. The
approaches vary:

- **QEMU** uses memory page dirty logging (`DIRTY_MEMORY_VGA`) plus
  display device callbacks (`dpy_gfx_update`). Efficient, but tied to
  its display subsystem.
- **TigerVNC** uses XDamage events, X drawing hooks (Xvnc), or
  platform-specific hooks (Windows WM hooks). Not applicable to VMs.
- **libvncserver** requires the application to call
  `rfbMarkRectAsModified()` explicitly.

We receive pixel-coordinate dirty rectangles directly from the Hyper-V
synthetic video device over the VMBus protocol. Windows guests and Linux
guests with `hyperv_drm` send `SYNTHVID_DIRT` messages. The server reads only
the dirty columns of the changed rows from VRAM, not whole scanlines.

The key advantage for our use case: on an idle 1080p desktop, the server
reads **0 bytes from VRAM per cycle** instead of scanning for changes.
For guests without device dirty support (older `hyperv_fb` driver), we
fall back to tile-based diffing automatically.

Because our dirty source is a cooperating guest driver, we can also turn it
off: when no client is connected, the worker asks the device to tell the
guest to stop reporting dirty rectangles (via the synthvid FeatureChange).
A VM with no viewer attached does no console dirty-tracking work until
someone connects.

### Non-ASCII Clipboard Paste Without Guest Agent

QEMU supports bidirectional VNC clipboard (since 6.1) but requires a
guest agent (`vdagent`) for guest integration. TigerVNC supports
clipboard via X selections or `vncconfig`. Both require guest-side
software.

We offer a different mechanism: Ctrl+Alt+P types clipboard contents
into the guest via keyboard emulation, requiring no guest agent at all.
ASCII characters use scancode injection. Non-ASCII Latin-1 characters
(umlauts, accented characters) use the Windows Alt+0+Numpad input method
targeting CP-1252. This works in any Windows application out of the box.

The tradeoff: this is one-directional (client to guest only), limited to
Latin-1 (no CJK), and slower than real clipboard integration for large
text. It is a pragmatic solution for the common case of typing passwords
and short text with special characters into a VM that has no guest tools
installed.

### Protocol Validation

QEMU validates `bits_per_pixel` (must be 8/16/32) but does not validate
shift values or max value conformance. It has had historical
vulnerabilities from malformed pixel format input. TigerVNC has thorough
validation via its `isSane()` function.

We validate comprehensively at SetPixelFormat time:
- `bits_per_pixel` must be 8, 16, or 32
- `true_color_flag` must be set
- Channel bit widths must not exceed 8
- `shift + channel_bits` must not exceed 32
- Max values must be non-zero
- Security type must match what was offered
- Non-conforming max values (not `2^N - 1`) are logged

Each validation has a dedicated error variant, with no panics on untrusted
input and no synthetic I/O errors.

### Pixel Format Conversion

QEMU uses `ctpopl()` (population count) to derive bit widths from max
values. This works correctly for conforming values (`2^N - 1`) but gives
wrong results for non-standard values. TigerVNC uses a custom
bit-scanning function equivalent to leading-zeros.

We use `leading_zeros()` (same approach as TigerVNC) and pre-compute all
shift and mask values once per connection in a `PixelConversion` struct.
The common case (32bpp little-endian XRGB, which most clients request)
hits a zero-copy fast path that skips per-pixel computation entirely.

### Output Batching

We accumulate the entire `FramebufferUpdate` message (header, cursor, all
rectangle headers and pixel data) into a single buffer and send it with one
`socket.write_all()`. QEMU also buffers output before flushing.
This is standard practice, not a unique advantage.

### Rectangle Merging

We merge adjacent dirty tiles into minimal rectangles via a two-pass
algorithm (horizontal spans, then vertical merge). QEMU also merges
dirty tiles using `find_next_bit`/`find_and_clear_dirty_height`.
libvncserver uses `sraSpanList` for region optimization. This is table
stakes for VNC servers, not a differentiator.

### Configurable Dirty-Tracking Tile Size

The dirty bitmap tracks change at a tile granularity: the screen is divided into
a grid of equal square tiles (8x8 by default), and a tile is the smallest unit
that can be marked dirty. This is separate from the encoding tile sizes the RFB
protocol fixes (Hextile is always 16x16, ZRLE always 64x64); it governs only how
finely the server detects and bounds a change before encoding it.

The size is a tradeoff. A dirty tile is read from VRAM and merged into a
tile-aligned rectangle, so the tile size sets the floor on how much is sent for
an isolated change: a one-pixel change inside a 16x16 tile still sends a 16x16
rectangle. Smaller tiles bound changes more tightly, sending less data for small,
scattered updates such as a text caret or a moving scrollbar, but they cost more
compute: more tiles to track, and the whole-framebuffer diff fallback compares
the screen at the tile stride every frame, so its cost grows roughly as the
screen area divided by the square of the tile size. Larger tiles are the reverse,
cheaper to scan but wasteful for small changes. The best choice depends on the
workload and the link: a busy, bandwidth-limited session leans toward small
tiles, while a CPU-limited host serving mostly full-screen video leans toward
large ones.

`--vnc-tile-size` exposes the knob (4, 8, or 16; default 8, the size that
measured best across resolutions and workloads).

Other servers fix this granularity rather than expose it:

- **QEMU** tracks dirty regions in a bitmap at a fixed 16-pixel-wide granularity
  (`VNC_DIRTY_PIXELS_PER_BIT = 16`).
- **TigerVNC** and **libvncserver** track change as regions (lists of
  rectangles) rather than a fixed tile grid, so there is no single tile-size
  setting; on the wire they use the protocol's fixed encoding tiles (16 for
  Hextile, 64 for ZRLE).

Measurement across resolutions and workloads put the sweet spot at 8 (lowest
CPU, near-minimal bandwidth), which is the default. The knob stays configurable
because that balance can shift with the client's encoding, the host's CPU
headroom, and the resolution, so an operator can retune without a code change.
The per-size numbers and method are in [Dirty-tracking tile size](./tile-size.md).

## Gaps and planned work

| Gap                | Who does it better | Why it matters                         |
|--------------------|--------------------|----------------------------------------|
| Authentication     | Everyone           | We have no auth at all yet             |
| Tight              | QEMU, TigerVNC     | Better photographic compression (JPEG) |
| WebSocket          | QEMU, libvnc       | noVNC needs a proxy to reach us        |
| Continuous updates | TigerVNC           | We still poll at 30ms intervals        |
| TLS encryption     | TigerVNC, QEMU     | We have no transport encryption        |
| Real cursor        | QEMU, TigerVNC     | We send a hardcoded arrow              |
| Extended clipboard | TigerVNC, QEMU     | We only support Latin-1 one-way        |

ZRLE is now implemented and preferred over `Zlib` for any client that offers it,
so it is no longer a gap. These remaining gaps are tracked for future work.
