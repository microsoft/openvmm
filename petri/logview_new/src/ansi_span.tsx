// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from "react";
import { ANSI_STYLE_MAP, ESC_REGEX } from "./utils/ansi";

/**
 * React component that parses ANSI SGR escape sequences in text and renders
 * them as styled <span> elements, mirroring the old ansiToSpan() function
 * from test.html.
 */
export function AnsiSpan({ text }: { text: string }): React.JSX.Element {
  // Fast path: no escape sequences at all
  if (!text.includes("\u001b")) {
    return <span>{text}</span>;
  }

  const parts: React.ReactNode[] = [];
  let lastIndex = 0;
  let currentStyles: React.CSSProperties = {};
  let key = 0;

  for (const match of text.matchAll(ESC_REGEX)) {
    const [fullMatch, codeStr] = match;
    const index = match.index!;

    // Emit plain text before this escape sequence
    if (index > lastIndex) {
      const segment = text.slice(lastIndex, index);
      if (Object.keys(currentStyles).length > 0) {
        parts.push(
          <span key={key++} style={{ ...currentStyles }}>
            {segment}
          </span>,
        );
      } else {
        parts.push(segment);
      }
    }

    // Update cumulative styles based on the SGR codes
    const codes = codeStr.split(";");
    for (const code of codes) {
      if (code === "0" || code === "") {
        // Reset all styles
        currentStyles = {};
        continue;
      }
      const style = ANSI_STYLE_MAP[code];
      if (style) {
        // Merge, overwriting any property of the same key
        currentStyles = { ...currentStyles, ...style };
      }
    }

    lastIndex = index + fullMatch.length;
  }

  // Emit any trailing text after the last escape sequence
  if (lastIndex < text.length) {
    const segment = text.slice(lastIndex);
    if (Object.keys(currentStyles).length > 0) {
      parts.push(
        <span key={key++} style={{ ...currentStyles }}>
          {segment}
        </span>,
      );
    } else {
      parts.push(segment);
    }
  }

  return <span>{parts}</span>;
}
