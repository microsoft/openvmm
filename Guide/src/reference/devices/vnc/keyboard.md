# VNC Keyboard Handling

## Overview

The openvmm VNC server handles keyboard input from VNC clients and translates
it into HID scancodes for the guest VM. There are two input paths depending
on the client's capabilities, and a clipboard paste feature that uses keyboard
emulation to type text into the guest.

## Input Paths

### 1. QEMU Extended Key Events (scancodes)

**Used by**: RealVNC, TigerVNC, noVNC (when QEMU extension is advertised)

The server advertises `ENCODING_TYPE_QEMU_EXTENDED_KEY_EVENT` (-258) during
encoding negotiation. Clients that support it send raw hardware scancodes
directly, bypassing keysym translation.

**Behavior**:
- Scancodes represent physical key positions, not characters
- The guest OS keyboard layout determines which character is produced
- Works correctly when client and guest keyboard layouts match
- Example: pressing the physical key for `Z` on a German QWERTZ keyboard
  sends scancode 0x2C, which the guest's German layout maps to `Y`

**Known issue**: RealVNC sends US-layout scancodes regardless of the client's
physical keyboard layout, producing wrong characters when the guest uses a
non-US layout. This is a RealVNC client bug, not a server issue.

### 2. Standard Keysym Events (RFB key events)

**Used by**: Clients that don't support QEMU extended key events (fallback)

The client sends X11 keysyms representing the intended character. The server
converts keysyms to US keyboard scancodes via `scancode.rs`.

**Behavior**:
- Keysyms carry the intended character (e.g. 0xF6 = `ö`)
- The `keysym_to_scancode()` function maps keysyms to US scancodes
- Only ASCII 32-126 and special keys (F1-F12, modifiers, arrows, etc.)
  are mapped; all other keysyms are silently dropped
- Shift state is tracked and injected/removed as needed

**Limitation**: Non-ASCII characters (umlauts, accented characters) cannot be
typed through this path because there are no US scancodes for them.

## Clipboard Paste (Ctrl+Alt+P)

The server intercepts `Ctrl+Alt+P` and types the clipboard contents into the
guest as keyboard input. The clipboard text is received from the client via
the RFB `ClientCutText` message.

### How Clients Send Clipboard

| Client   | Clipboard Behavior                                              |
|----------|-----------------------------------------------------------------|
| RealVNC  | Auto-sends on copy, or via right-click "Transfer clipboard"     |
| TigerVNC | Auto-sends, but Ctrl+Alt+P is intercepted (see below)           |
| noVNC    | Manual: open sidebar, click clipboard icon, paste into text box |

### Paste Mechanism

**ASCII characters** (32-126): Converted via keysym-to-scancode mapping,
sent as key press/release events.

**Non-ASCII Latin-1 characters** (128-255): Uses the Windows Alt+Numpad
input method:
1. Press Alt
2. Type `0` + decimal codepoint on numpad (e.g. `0246` for `ö`)
3. Release Alt

The leading `0` selects CP-1252 (Windows-1252) encoding. Without it,
Windows uses the OEM codepage (CP-437) which maps different characters.

**Characters beyond Latin-1** (> 255): Silently skipped.

### Ctrl+Alt+P Detection

Tracked in both input paths:
- **Keysym path**: Checks keysyms 0xFFE3 (Ctrl_L), 0xFFE9 (Alt_L), and
  both `p` (0x70) and `P` (0x50)
- **QEMU path**: Checks scancodes 0x1D (Ctrl), 0x38 (Alt), 0x19 (P)

### TigerVNC Limitation

TigerVNC intercepts `Ctrl+Alt` as its own menu combination. The `P`
keystroke never reaches the server. Clipboard paste does not work in
TigerVNC. Use RealVNC or noVNC instead.

## Client Compatibility Matrix

| Feature             | RealVNC     | TigerVNC     | noVNC        | MobaXterm |
|---------------------|-------------|--------------|--------------|-----------|
| Keyboard layout     | US only (*) | Correct      | Correct      | TBA       |
| Ctrl+Alt+P paste    | Works       | Blocked (**) | Works        | TBA       |
| Umlaut paste (öäü)  | Works       | N/A          | Works        | TBA       |
| Auto clipboard send | Yes         | Yes          | Manual (***) | TBA       |
| QEMU extended keys  | Yes         | Yes          | Yes          | TBA       |

(*) RealVNC client bug: sends US scancodes regardless of client keyboard  
(**) TigerVNC intercepts Ctrl+Alt before sending P  
(***) noVNC requires manual clipboard entry via sidebar panel

## Why RealVNC Cannot Be Fixed Server-Side

RealVNC sends QEMU extended key events with US-layout scancodes regardless
of the client's physical keyboard. The keysym field in the QEMU event
carries the intended character, but using it to remap scancodes via the
US keysym-to-scancode table produces wrong results for non-US layouts:

Example: German `"` (Shift+2) → keysym 0x22 → US scancode 0x28+Shift →
German guest maps 0x28+Shift to `Ä` (wrong!)

The keysym-to-scancode table in `scancode.rs` only maps to US keyboard
positions. A `--vnc-keyboard-layout` parameter with per-layout scancode
tables would fix this but is not yet implemented.

## Known Limitations

- RealVNC sends US-layout scancodes regardless of client keyboard layout
- TigerVNC intercepts Ctrl+Alt before sending P (paste does not work)
- Non-ASCII characters beyond Latin-1 cannot be typed via keysym path
