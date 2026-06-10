# Dirty-tracking tile size

The VNC server detects framebuffer changes at a tile granularity: the screen is
divided into a grid of equal square tiles, and a tile is the smallest unit that
can be marked dirty. `--vnc-tile-size` sets the tile edge length in pixels. This
page records the measurements behind the default and the offered set.

Tile size is a detection granularity, not a wire format. A dirty tile is read
from VRAM and merged into a tile-aligned rectangle, so the tile size sets the
floor on how much is sent for an isolated change: a one-pixel change inside a
16x16 tile still sends a 16x16 rectangle. Smaller tiles bound a change more
tightly (fewer wire bytes for small, scattered updates such as a text caret) but
cost more compute: more tiles to track, and the whole-framebuffer diff fallback
scans at the tile stride every frame, so its cost grows roughly as the screen
area over the square of the tile size. This is separate from the encoding tile
sizes the RFB protocol fixes (Hextile is always 16x16, ZRLE 64x64).

## Default and offered sizes

`--vnc-tile-size` accepts `4`, `8`, or `16`, defaulting to `8`. Measurement put
the balance of bandwidth and CPU at 8 across resolutions and workloads. Sizes of
2 and 32 were measured and left out of the offered set: 2 costs 2 to 3 times the
CPU with no bandwidth gain (and is worse on bytes at higher resolution), and 32
costs more on both axes.

The size is fixed for a connection; there is no adaptive mode. A single fixed 8
captured nearly all of the available benefit in testing, and changing the size
mid-session forces a full retransmit, so an adaptive controller was not worth the
complexity. The knob stays configurable because the balance can move with the
client's encoding, the host's CPU headroom, and the resolution.

A `cycle` value is available as a diagnostic. It rotates the tile size through 2,
4, 8, 16, 32 every 30 seconds and logs the bytes and CPU spent at each, for
re-measuring the tradeoff on a given workload. It is not a normal operating mode.

## Measurements

Per-frame, steady state (the forced full-refresh frame after each cycle switch is
excluded). Static rows are bytes/frame and microseconds/frame; video rows are
kilobytes/frame and milliseconds/frame. Tile 8 (bold) is the CPU minimum in every
static row.

| workload (units)          | tile 2     | tile 4     | tile 8         | tile 16    | tile 32    |
|---------------------------|------------|------------|----------------|------------|------------|
| 1024x768 static (B / us)  | 191 / 432  | 185 / 226  | **185 / 196**  | 266 / 249  | 268 / 306  |
| 1280x1024 static (B / us) | 245 / 587  | 269 / 314  | **274 / 241**  | 359 / 292  | 389 / 370  |
| 1600x1200 static (B / us) | 354 / 771  | 314 / 376  | **329 / 274**  | 401 / 339  | 428 / 409  |
| 1920x1080 static (B / us) | 437 / 1038 | 423 / 517  | **405 / 363**  | 586 / 439  | 573 / 599  |
| 1024x768 video (KB / ms)  | 307 / 29.1 | 304 / 24.8 | 313 / 24.9     | 304 / 25.6 | 304 / 32.6 |
| 1280x1024 video (KB / ms) | 220 / 34.3 | 221 / 29.3 | 221 / 30.9     | 221 / 30.7 | 224 / 33.6 |
| 1600x1200 video (KB / ms) | 376 / 44.1 | 375 / 43.6 | 376 / 44.7     | 374 / 44.6 | 370 / 43.7 |
| 1920x1080 video (KB / ms) | 252 / 45.7 | 247 / 45.5 | **246 / 42.8** | 251 / 46.0 | 250 / 46.4 |

### Static (light, cursor-only workload)

CPU is a clean U-shape with the minimum at tile 8 at all four resolutions, the
extremes scaling up with pixel count (tile 2 reaches 2.8 to 2.9 times the tile-8
CPU at the larger modes). Bandwidth rises with tile size at the top end (the 16/32
jump is consistent), but at the small end the "smaller is fewer bytes" trend
reverses as resolution grows: the per-rectangle ZRLE header overhead from many
tiny tiles overtakes the tighter bounding, so the byte-optimal tile drifts up
2 to 4 to 8 with resolution. At 1920x1080 tile 8 is the minimum on both axes at
once.

### Video (full-screen, constant-rate loop)

Tile size moves neither bytes nor CPU at any resolution (variation within a few
percent is noise). The full-frame ZRLE encode dominates, and the mild U still
visible at 1024x768 (tile 32 about 30% over the minimum) washes out by 1600x1200.
Because the loop runs at a constant change rate, the video runs are comparable
across resolutions:

- CPU/frame scales roughly linearly with pixel count (25 / 31 / 44 / 45 ms at
  0.79M / 1.31M / 1.92M / 2.07M pixels), as expected for a per-pixel encode. Frame
  rate falls with resolution (10.8 to 8.6 fps) because the client is saturated
  throughout every video run; the per-client dirty channel fills and the server
  recovers with a full refresh, which is the designed backpressure.
- Delivered bandwidth clusters at about 3.2 versus 2.2 MB/s and tracks aspect
  ratio, not pixel count: the 4:3 modes (1024x768, 1600x1200) sit at about 3.2
  MB/s, the 5:4 and 16:9 modes at about 2.2. Consistent with a 4:3 clip that fills
  a 4:3 screen edge to edge but is pillar/letterboxed on 5:4 and 16:9, leaving
  static bars and less moving area.

Tile 8 is best or tied-best in every cell that matters, with no remaining tradeoff
at the top resolution. That is the basis for the default and for leaving out an
adaptive mode.

## Method

`--vnc-tile-size cycle` rotates the tile size 2, 4, 8, 16, 32 every 30 seconds and
logs, per period: `bytes_sent` (server-to-client wire bytes), `frames`, and
`proc_micros` (wall time of dirty collection plus encode, the tile-size-sensitive
compute, excluding the socket write). The forced full-refresh frame after each
switch is excluded so the numbers are steady state, not the one-off full
retransmit. Each run used a Windows guest and one client negotiating ZRLE. The
static workload was an idle desktop with a blinking cursor; the video workload was
the same looped clip played full-screen at each resolution.

Two classes of sample were treated as transients and excluded, with clean repeat
samples used instead: the first video period after a static-to-video transition
(the whole screen changing at once, about 1.7 to 2 times the steady byte rate),
and one tile-32 static sample taken during a resolution change.
