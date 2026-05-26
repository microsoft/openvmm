// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/**
 * Map of ANSI SGR codes to CSS style properties.
 * Covers text styles (bold, italic, underline) and standard 8/16 foreground colors.
 */
export const ANSI_STYLE_MAP: Record<string, React.CSSProperties> = {
  // Text styles
  "1": { fontWeight: "bold" },
  "3": { fontStyle: "italic" },
  "4": { textDecoration: "underline" },

  // Standard foreground colors (30–37)
  "30": { color: "black" },
  "31": { color: "red" },
  "32": { color: "green" },
  "33": { color: "#b58900" },
  "34": { color: "blue" },
  "35": { color: "magenta" },
  "36": { color: "cyan" },
  "37": { color: "white" },

  // Bright foreground colors (90–97)
  "90": { color: "gray" },
  "91": { color: "lightcoral" },
  "92": { color: "lightgreen" },
  "93": { color: "gold" },
  "94": { color: "lightskyblue" },
  "95": { color: "plum" },
  "96": { color: "lightcyan" },
  "97": { color: "white" },

  // Reset foreground
  "39": { color: "inherit" },
};

export const ESC_REGEX = /\u001b\[([0-9;]*)m/g;

/**
 * Strips all ANSI SGR escape sequences from a string, returning plain text.
 * Useful for search, filtering, and clipboard copy.
 */
export function stripAnsi(str: string): string {
  return str.replace(ESC_REGEX, "");
}
