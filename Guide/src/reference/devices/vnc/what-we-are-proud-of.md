# Where This VNC Server Stands

An honest comparison with QEMU, TigerVNC, and libvncserver. No inflated
claims -- just what we actually do differently and where others are ahead.

## What We Do Well

### Per-Client Isolation in Multi-Client Mode

QEMU, TigerVNC, and libvncserver all support multiple concurrent clients.
We do too (up to `--vnc-max-clients`, default 16). What distinguishes our
implementation is the degree of per-client isolation:

- Independent pixel format per client (8bpp and 32bpp viewers simultaneously)
- Separate zlib compression stream per client (RFB requires this, but not
  all implementations get it right under concurrency)
- Independent framebuffer snapshots and dirty tracking per client
- Independent encoding negotiation (one client can use zlib, another raw)
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
guests with `hyperv_drm` send `SYNTHVID_DIRT` messages. The server reads
only the affected scanlines from VRAM.

The key advantage for our use case: on an idle 1080p desktop, the server
reads **0 bytes from VRAM per cycle** instead of scanning for changes.
For guests without device dirty support (older `hyperv_fb` driver), we
fall back to tile-based diffing automatically.

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

Each validation has a dedicated error variant -- no panics on untrusted
input, no synthetic I/O errors.

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

We accumulate the entire `FramebufferUpdate` message -- header, cursor,
all rectangle headers and pixel data -- into a single buffer and send it
with one `socket.write_all()`. QEMU also buffers output before flushing.
This is standard practice, not a unique advantage.

### Rectangle Merging

We merge adjacent dirty tiles into minimal rectangles via a two-pass
algorithm (horizontal spans, then vertical merge). QEMU also merges
dirty tiles using `find_next_bit`/`find_and_clear_dirty_height`.
libvncserver uses `sraSpanList` for region optimization. This is table
stakes for VNC servers, not a differentiator.

## What We Do Not Do Better (Yet)

| Gap                | Who does it better | Why it matters                       |
|--------------------|--------------------|--------------------------------------|
| Authentication     | Everyone           | We have no auth at all yet           |
| Tight/ZRLE         | QEMU, TigerVNC     | Better compression for slow networks |
| WebSocket          | QEMU, libvnc       | noVNC needs a proxy to reach us      |
| Continuous updates | TigerVNC           | We still poll at 30ms intervals      |
| TLS encryption     | TigerVNC, QEMU     | We have no transport encryption      |
| Real cursor        | QEMU, TigerVNC     | We send a hardcoded arrow            |
| Extended clipboard | TigerVNC, QEMU     | We only support Latin-1 one-way      |

These gaps are tracked for future work.
