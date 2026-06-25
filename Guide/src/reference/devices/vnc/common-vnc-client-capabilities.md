# Common VNC client capabilities

The RFB protocol has no field that names the client software. A client only
announces what it can do, through the `SetEncodings` message, which lists the
framebuffer encodings and pseudo-encodings it understands. The contents of that
list are characteristic of each client, so the server uses them to make a
best-effort guess at which client connected (logged as `client_guess` on the
`client encodings` line; the full decoded list is logged at `debug` as
`client offered encodings`).

This page is a capability matrix: what each common client advertises, what the
OpenVMM VNC server implements, and what QEMU's built-in VNC server implements,
for comparison. It starts with framebuffer encodings and will grow to cover
keyboard, pointer, and other capabilities.

## How the data was gathered

Each client connected to the server running with
`OPENVMM_LOG=info,vnc=debug,vnc_worker=debug`, and the `client offered encodings`
line was read back (every code is decoded to a name by `encoding_name` in
`rfb.rs`). All clients below negotiated RFB version `003.008`.

The QEMU column is taken from QEMU's `ui/vnc.c` `set_encodings` (the encodings the
server recognizes) and `ui/vnc.h`, so it is exact rather than observed. Three of
its capabilities are conditional: `TightPNG` is compiled only with libpng
(`CONFIG_PNG`), `QEMUAudio` needs an audio backend, and `xvp` needs power control.

Legend: `✓` means the capability is present (for a client, that it advertised the
encoding; for a server, that the server implements it); `✗` means absent; `?`
marks a cell that has not been verified (there are none below).

The rows are the registered encoding and pseudo-encoding numbers from the IANA
Remote Framebuffer (RFB) registry
(<https://www.iana.org/assignments/rfb/rfb.xhtml>), the same set `encoding_name`
in `rfb.rs` decodes. Numbers in parentheses are the registry value.

## Framebuffer encodings

| Encoding       | noVNC | TigerVNC | RealVNC | MobaXterm | UltraVNC | OpenVMM | QEMU |
|----------------|:-----:|:--------:|:-------:|:---------:|:--------:|:-------:|:----:|
| Raw (0)        | ✓     | ✓        | ✓       | ✓         | ✓        | ✓       | ✓    |
| CopyRect (1)   | ✓     | ✓        | ✓       | ✓         | ✓        | ✗       | ✗    |
| RRE (2)        | ✓     | ✓        | ✓       | ✗         | ✓        | ✗       | ✗    |
| CoRRE (4)      | ✗     | ✗        | ✗       | ✗         | ✓        | ✗       | ✗    |
| Hextile (5)    | ✓     | ✓        | ✓       | ✗         | ✓        | ✗       | ✓    |
| Zlib (6)       | ✓     | ✗        | ✓       | ✓         | ✓        | ✓       | ✓    |
| Tight (7)      | ✓     | ✓        | ✗       | ✗         | ✓        | ✗       | ✓    |
| ZlibHex (8)    | ✗     | ✗        | ✗       | ✗         | ✓        | ✗       | ✗    |
| TRLE (15)      | ✗     | ✗        | ✓       | ✓         | ✗        | ✗       | ✗    |
| ZRLE (16)      | ✓     | ✓        | ✓       | ✓         | ✓        | ✓       | ✓    |
| ZYWRLE (17)    | ✗     | ✗        | ✗       | ✗         | ✓        | ✗       | ✓    |
| H.264 (20)     | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| JPEG (21)      | ✓     | ✓        | ✓       | ✗         | ✗        | ✗       | ✗    |
| JRLE (22)      | ✗     | ✗        | ✓       | ✗         | ✗        | ✗       | ✗    |
| VA-H.264 (23)  | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| ZRLE2 (24)     | ✗     | ✗        | ✓       | ✗         | ✗        | ✗       | ✗    |
| OpenH.264 (50) | ✗     | ✓        | ✗       | ✗         | ✗        | ✗       | ✗    |

## Pseudo-encodings

| Pseudo-encoding                | noVNC | TigerVNC | RealVNC | MobaXterm | UltraVNC | OpenVMM | QEMU |
|--------------------------------|:-----:|:--------:|:-------:|:---------:|:--------:|:-------:|:----:|
| DesktopSize (-223)             | ✓     | ✓        | ✓       | ✗         | ✓        | ✓       | ✓    |
| LastRect (-224)                | ✓     | ✓        | ✗       | ✗         | ✓        | ✗       | ✗    |
| PointerPos (-232)              | ✗     | ✗        | ✗       | ✗         | ✓        | ✗       | ✗    |
| Cursor (-239)                  | ✓     | ✓        | ✓       | ✓         | ✓        | ✓       | ✓    |
| XCursor (-240)                 | ✗     | ✓        | ✗       | ✗         | ✗        | ✗       | ✗    |
| QEMUPointerMotionChange (-257) | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✓    |
| QEMUExtendedKeyEvent (-258)    | ✓     | ✓        | ✗       | ✗         | ✗        | ✓       | ✓    |
| QEMUAudio (-259)               | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✓    |
| TightPNG (-260)                | ✓     | ✗        | ✗       | ✗         | ✗        | ✗       | ✓    |
| LedState (-261)                | ✓     | ✓        | ✗       | ✗         | ✗        | ✗       | ✓    |
| gii (-305)                     | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| popa (-306)                    | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| DesktopName (-307)             | ✓     | ✓        | ✗       | ✗         | ✗        | ✗       | ✗    |
| ExtendedDesktopSize (-308)     | ✓     | ✓        | ✗       | ✗         | ✓        | ✗       | ✓    |
| xvp (-309)                     | ✓     | ✗        | ✗       | ✗         | ✗        | ✗       | ✓    |
| OliveCallControl (-310)        | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| ClientRedirect (-311)          | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| Fence (-312)                   | ✓     | ✓        | ✗       | ✗         | ✗        | ✗       | ✗    |
| ContinuousUpdates (-313)       | ✓     | ✓        | ✗       | ✗         | ✗        | ✗       | ✗    |
| CursorWithAlpha (-314)         | ✗     | ✓        | ✓       | ✗         | ✗        | ✗       | ✓    |
| ColorMap (-315)                | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |
| ExtendedMouseButtons (-316)    | ✓     | ✓        | ✗       | ✗         | ✗        | ✗       | ✗    |
| TightNoZlib (-317)             | ✗     | ✗        | ✗       | ✗         | ✗        | ✗       | ✗    |

## Tight option ranges and vendor blocks

The registry also reserves Tight option ranges (negotiation parameters, not
capabilities) and vendor blocks. They are not rows above; this is where each
client's offer landed:

- Tight option pseudo-encodings (`TightCompressionLevel`, `TightJpegQuality`, and
  the finer `TightFineQuality`, `TightSubsampling`): offered by every client that
  offers `Tight` (noVNC, TigerVNC, UltraVNC) to pin its preferred quality. QEMU
  reads `TightCompressionLevel` and `TightJpegQuality`.
- `ExtendedClipboard` (`0xc0a1e5ce..cf`): noVNC, TigerVNC, UltraVNC, and QEMU
  (`CLIPBOARD_EXT`).
- `VMware (0x574d56xx)`: TigerVNC offers three; noVNC offers one. QEMU implements
  the `WMVi` member for desktop resize.
- `UltraVNC (0xffff0000..0xffff8003)`: UltraVNC only. Its unmistakable marker.
- `RealVNC (1024..1099)`, `Apple (1000..1002, 1011, 1100..1109)`, `LibVNCServer
  (0xfffe00xx)`, `CarConnectivity`: none of the profiled clients advertised these.

## What OpenVMM uses

OpenVMM implements `Raw`, `Zlib`, and `ZRLE` for the wire (plus the
`DesktopSize`, `Cursor`, and `QEMUExtendedKeyEvent` pseudo-encodings). It prefers
`ZRLE` for any client that advertises it, uses `Zlib` for a client that offers
the plain `zlib` encoding but not `ZRLE`, and `Raw` otherwise. The other
advertised encodings (`Tight`, `Hextile`, and the rest) are not implemented.

Every profiled client offers `ZRLE`, so each now negotiates a compressed
encoding. This closes the worst case, TigerVNC: it offers `ZRLE` and `Tight` but
not the plain `zlib` encoding, so it used to fall back to uncompressed `Raw`, and
now gets `ZRLE`.

For comparison, QEMU picks the client's most-preferred supported encoding and
also prefers `ZRLE` over plain `Zlib`: `Tight` for noVNC and TigerVNC (they list
`Tight` first), `ZRLE` for RealVNC, MobaXterm, and UltraVNC (they list `ZRLE`
first).

## Identifying the client

`guess_client` in `rfb.rs` checks the offered set in priority order, most
specific signature first:

1. an encoding in the UltraVNC vendor range `0xffff8000..=0xffff8003` -> UltraVNC
2. `TightPNG` -> noVNC
3. `OpenH.264` and `XCursor` -> TigerVNC
4. `ZRLE2` or `JRLE` -> RealVNC
5. no `Tight`, no `DesktopSize`, but `TRLE` present, in a small offer (8 or
   fewer encodings) -> MobaXterm
6. otherwise no guess

The order matters: UltraVNC also offers `Tight`, and TigerVNC offers `ZRLE`, so
the vendor and PNG markers are tested before the more generic shapes.

## Caveats

The guess is best-effort, not an identity:

- It is version-dependent. A client's encoding set changes across releases.
- Clients built on a shared library (libvncclient, the TightVNC core) can share a
  signature and be indistinguishable.
- A client that matches no signature gets no guess and is not misreported.

The full offered list is always logged at `debug`, so a connection can be
identified by hand even when the heuristic abstains.

## Clients not yet profiled

These have not been captured, so they fall through to no guess and log their full
encoding list for later classification: TightVNC, Remmina, Vinagre, the macOS
built-in Screen Sharing client (Apple Remote Desktop), the RealVNC viewer on
platforms other than the one tested here, and web clients other than noVNC.
Adding one is a matter of connecting it, reading its `client offered encodings`
line, and extending the tables and `guess_client` with a distinguishing marker.
