// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides machinery to convert from the xkeysym keyboard input
//! format used by RFB to US keyboard scancodes used by VMs.

/// If set on a scancode value, a shift key must be held to emit the desired
/// character.
const SHIFT: u32 = 0x10000;
/// If set on a scancode value, there must be no shift key held in order to emit
/// the desired character.
const UNSHIFT: u32 = 0x20000;

const ASCII_PRINT_START: u16 = 32;

/// This table maps ASCII values to US keyboard scancodes, starting at
/// ASCII_PRINT_START.
const ASCII_TO_US: &[u32] = &[
    0x39,           // 32 ' '
    0x02 | SHIFT,   // 33 !
    0x28 | SHIFT,   // 34 "
    0x04 | SHIFT,   // 35 #
    0x05 | SHIFT,   // 36 $
    0x06 | SHIFT,   // 37 %
    0x08 | SHIFT,   // 38 &
    0x28 | UNSHIFT, // 39 '
    0x0a | SHIFT,   // 40 (
    0x0b | SHIFT,   // 41 )
    0x09 | SHIFT,   // 42 *
    0x0d | SHIFT,   // 43 +
    0x33 | UNSHIFT, // 44 ,
    0x0c | UNSHIFT, // 45 -
    0x34 | UNSHIFT, // 46 .
    0x35 | UNSHIFT, // 47 /
    0x0b | UNSHIFT, // 48 0
    0x02 | UNSHIFT, // 49 1
    0x03 | UNSHIFT, // 50 2
    0x04 | UNSHIFT, // 51 3
    0x05 | UNSHIFT, // 52 4
    0x06 | UNSHIFT, // 53 5
    0x07 | UNSHIFT, // 54 6
    0x08 | UNSHIFT, // 55 7
    0x09 | UNSHIFT, // 56 8
    0x0a | UNSHIFT, // 57 9
    0x27 | SHIFT,   // 58 :
    0x27 | UNSHIFT, // 59 ;
    0x33 | SHIFT,   // 60 <
    0x0d | UNSHIFT, // 61 =
    0x34 | SHIFT,   // 62 >
    0x35 | SHIFT,   // 63 ?
    0x03 | SHIFT,   // 64 @
    0x1e | SHIFT,   // 65 A
    0x30 | SHIFT,   // 66 B
    0x2e | SHIFT,   // 67 C
    0x20 | SHIFT,   // 68 D
    0x12 | SHIFT,   // 69 E
    0x21 | SHIFT,   // 70 F
    0x22 | SHIFT,   // 71 G
    0x23 | SHIFT,   // 72 H
    0x17 | SHIFT,   // 73 I
    0x24 | SHIFT,   // 74 J
    0x25 | SHIFT,   // 75 K
    0x26 | SHIFT,   // 76 L
    0x32 | SHIFT,   // 77 M
    0x31 | SHIFT,   // 78 N
    0x18 | SHIFT,   // 79 O
    0x19 | SHIFT,   // 80 P
    0x10 | SHIFT,   // 81 Q
    0x13 | SHIFT,   // 82 R
    0x1f | SHIFT,   // 83 S
    0x14 | SHIFT,   // 84 T
    0x16 | SHIFT,   // 85 U
    0x2f | SHIFT,   // 86 V
    0x11 | SHIFT,   // 87 W
    0x2d | SHIFT,   // 88 X
    0x15 | SHIFT,   // 89 Y
    0x2c | SHIFT,   // 90 Z
    0x1a | UNSHIFT, // 91 [
    0x2b | UNSHIFT, // 92 '\'
    0x1b | UNSHIFT, // 93 ]
    0x07 | SHIFT,   // 94 ^
    0x0c | SHIFT,   // 95 _
    0x29 | UNSHIFT, // 96 `
    0x1e | UNSHIFT, // 97 a
    0x30 | UNSHIFT, // 98 b
    0x2e | UNSHIFT, // 99 c
    0x20 | UNSHIFT, // 100 d
    0x12 | UNSHIFT, // 101 e
    0x21 | UNSHIFT, // 102 f
    0x22 | UNSHIFT, // 103 g
    0x23 | UNSHIFT, // 104 h
    0x17 | UNSHIFT, // 105 i
    0x24 | UNSHIFT, // 106 j
    0x25 | UNSHIFT, // 107 k
    0x26 | UNSHIFT, // 108 l
    0x32 | UNSHIFT, // 109 m
    0x31 | UNSHIFT, // 110 n
    0x18 | UNSHIFT, // 111 o
    0x19 | UNSHIFT, // 112 p
    0x10 | UNSHIFT, // 113 q
    0x13 | UNSHIFT, // 114 r
    0x1f | UNSHIFT, // 115 s
    0x14 | UNSHIFT, // 116 t
    0x16 | UNSHIFT, // 117 u
    0x2f | UNSHIFT, // 118 v
    0x11 | UNSHIFT, // 119 w
    0x2d | UNSHIFT, // 120 x
    0x15 | UNSHIFT, // 121 y
    0x2c | UNSHIFT, // 122 z
    0x1a | SHIFT,   // 123 {
    0x2b | SHIFT,   // 124 |
    0x1b | SHIFT,   // 125 }
    0x29 | SHIFT,   // 126 ~
];

/// X keysyms (other than the ones that match ASCII values).
const KEYSYM_BACK_SPACE: u16 = 0xff08;
const KEYSYM_TAB: u16 = 0xff09;
const KEYSYM_RETURN_OR_ENTER: u16 = 0xff0d;
const KEYSYM_ESCAPE: u16 = 0xff1b;
const KEYSYM_INSERT: u16 = 0xff63;
const KEYSYM_DELETE: u16 = 0xffff;
const KEYSYM_HOME: u16 = 0xff50;
const KEYSYM_END: u16 = 0xff57;
const KEYSYM_PAGE_UP: u16 = 0xff55;
const KEYSYM_PAGE_DOWN: u16 = 0xff56;
const KEYSYM_LEFT: u16 = 0xff51;
const KEYSYM_UP: u16 = 0xff52;
const KEYSYM_RIGHT: u16 = 0xff53;
const KEYSYM_DOWN: u16 = 0xff54;
const KEYSYM_F1: u16 = 0xffbe;
const KEYSYM_F2: u16 = 0xffbf;
const KEYSYM_F3: u16 = 0xffc0;
const KEYSYM_F4: u16 = 0xffc1;
const KEYSYM_F5: u16 = 0xffc2;
const KEYSYM_F6: u16 = 0xffc3;
const KEYSYM_F7: u16 = 0xffc4;
const KEYSYM_F8: u16 = 0xffc5;
const KEYSYM_F9: u16 = 0xffc6;
const KEYSYM_F10: u16 = 0xffc7;
const KEYSYM_F11: u16 = 0xffc8;
const KEYSYM_F12: u16 = 0xffc9;
const KEYSYM_SHIFT_LEFT: u16 = 0xffe1;
const KEYSYM_SHIFT_RIGHT: u16 = 0xffe2;
const KEYSYM_CONTROL_LEFT: u16 = 0xffe3;
const KEYSYM_CONTROL_RIGHT: u16 = 0xffe4;
const KEYSYM_META_LEFT: u16 = 0xffe7;
const KEYSYM_META_RIGHT: u16 = 0xffe8;
const KEYSYM_ALT_LEFT: u16 = 0xffe9;
const KEYSYM_ALT_RIGHT: u16 = 0xffea;

/// Table mapping non-ASCII xkeysyms to US keyboard scancodes.
const KEYSYM_TO_US: &[(u16, u32)] = &[
    (KEYSYM_BACK_SPACE, 0x0e),
    (KEYSYM_TAB, 0x0f),
    (KEYSYM_RETURN_OR_ENTER, 0x1c),
    (KEYSYM_ESCAPE, 0x01),
    (KEYSYM_INSERT, 0xe052),
    (KEYSYM_DELETE, 0xe053),
    (KEYSYM_HOME, 0xe047),
    (KEYSYM_END, 0xe04f),
    (KEYSYM_PAGE_UP, 0xe049),
    (KEYSYM_PAGE_DOWN, 0xe051),
    (KEYSYM_LEFT, 0xe04b),
    (KEYSYM_UP, 0xe048),
    (KEYSYM_RIGHT, 0xe04d),
    (KEYSYM_DOWN, 0xe050),
    (KEYSYM_F1, 0x3b),
    (KEYSYM_F2, 0x3d),
    (KEYSYM_F3, 0x3e),
    (KEYSYM_F4, 0x3f),
    (KEYSYM_F5, 0x40),
    (KEYSYM_F6, 0x41),
    (KEYSYM_F7, 0x42),
    (KEYSYM_F8, 0x43),
    (KEYSYM_F9, 0x44),
    (KEYSYM_F10, 0x45),
    (KEYSYM_F11, 0x57),
    (KEYSYM_F12, 0x58),
    (KEYSYM_SHIFT_LEFT, 0x2a),
    (KEYSYM_SHIFT_RIGHT, 0x36),
    (KEYSYM_CONTROL_LEFT, 0x1d),
    (KEYSYM_CONTROL_RIGHT, 0xe01d),
    (KEYSYM_META_LEFT, 0xe05b),
    (KEYSYM_META_RIGHT, 0xe05c),
    (KEYSYM_ALT_LEFT, 0x38),
    (KEYSYM_ALT_RIGHT, 0xe038),
];

/// Converts an xkeysym to a US keyboard scancode (possibly with SHIFT or
/// UNSHIFT set). Returns None if there is no such mapping.
fn keysym_to_scancode(keysym: u16) -> Option<u32> {
    if keysym >= ASCII_PRINT_START && ((keysym - ASCII_PRINT_START) as usize) < ASCII_TO_US.len() {
        Some(ASCII_TO_US[(keysym - ASCII_PRINT_START) as usize])
    } else {
        KEYSYM_TO_US
            .iter()
            .find_map(|(ks, code)| if keysym == *ks { Some(*code) } else { None })
    }
}

/// Scancode tracking state.
pub struct State {
    lshift: bool,
    rshift: bool,
}

impl State {
    /// Constructs a new State.
    pub fn new() -> Self {
        Self {
            lshift: false,
            rshift: false,
        }
    }

    /// Emits scancodes (by calling `f`) corresponding to the provided ASCII char.
    /// Panics if `c` is outside of the ASCII printable range (' ' to '~').
    pub fn emit_ascii_char<F: FnMut(u16, bool)>(&mut self, c: u8, down: bool, f: F) {
        self.emit_us_scancode(ASCII_TO_US[(c - ASCII_PRINT_START as u8) as usize], down, f)
    }

    /// Emits scancodes (by calling `f`) corresponding to the provided US keyboard scancode.
    pub fn emit_us_scancode<F: FnMut(u16, bool)>(&mut self, scancode: u32, down: bool, mut f: F) {
        if down {
            if scancode & SHIFT != 0 && !self.lshift && !self.rshift {
                let lshift = keysym_to_scancode(KEYSYM_SHIFT_LEFT).unwrap() as u16;
                f(lshift, true);
                f(scancode as u16, true);
                f(lshift, false);
            } else if scancode & UNSHIFT != 0 && (self.lshift || self.rshift) {
                let lshift = keysym_to_scancode(KEYSYM_SHIFT_LEFT).unwrap() as u16;
                let rshift = keysym_to_scancode(KEYSYM_SHIFT_RIGHT).unwrap() as u16;
                if self.lshift {
                    f(lshift, false);
                }
                if self.rshift {
                    f(rshift, false);
                }
                f(scancode as u16, true);
                if self.lshift {
                    f(lshift, true);
                }
                if self.rshift {
                    f(rshift, true);
                }
            } else {
                f(scancode as u16, true);
            }
        } else {
            f(scancode as u16, false);
        }
    }

    /// Emits scancodes (by calling `f`) corresponding to the provided xkeysym.
    pub fn emit<F: FnMut(u16, bool)>(&mut self, keysym: u16, down: bool, f: F) {
        if let Some(scancode) = keysym_to_scancode(keysym) {
            self.emit_us_scancode(scancode, down, f);

            if keysym == KEYSYM_SHIFT_LEFT {
                self.lshift = down;
            }
            if keysym == KEYSYM_SHIFT_RIGHT {
                self.rshift = down;
            }
        }
    }
}
