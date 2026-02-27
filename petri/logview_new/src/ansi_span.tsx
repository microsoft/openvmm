// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from 'react';

/**
 * Map of ANSI SGR codes to CSS style properties.
 * Covers text styles (bold, italic, underline) and standard 8/16 foreground colors.
 */
const ANSI_STYLE_MAP: Record<string, React.CSSProperties> = {
    // Text styles
    '1': { fontWeight: 'bold' },
    '3': { fontStyle: 'italic' },
    '4': { textDecoration: 'underline' },

    // Standard foreground colors (30–37)
    '30': { color: 'black' },
    '31': { color: 'red' },
    '32': { color: 'green' },
    '33': { color: '#b58900' },
    '34': { color: 'blue' },
    '35': { color: 'magenta' },
    '36': { color: 'cyan' },
    '37': { color: 'white' },

    // Bright foreground colors (90–97)
    '90': { color: 'gray' },
    '91': { color: 'lightcoral' },
    '92': { color: 'lightgreen' },
    '93': { color: 'gold' },
    '94': { color: 'lightskyblue' },
    '95': { color: 'plum' },
    '96': { color: 'lightcyan' },
    '97': { color: 'white' },

    // Reset foreground
    '39': { color: 'inherit' },
};

const ESC_REGEX = /\u001b\[([0-9;]*)m/g;

/**
 * Strips all ANSI SGR escape sequences from a string, returning plain text.
 * Useful for search, filtering, and clipboard copy.
 */
export function stripAnsi(str: string): string {
    return str.replace(ESC_REGEX, '');
}

/**
 * React component that parses ANSI SGR escape sequences in text and renders
 * them as styled <span> elements, mirroring the old ansiToSpan() function
 * from test.html.
 */
export function AnsiSpan({ text }: { text: string }): React.JSX.Element {
    // Fast path: no escape sequences at all
    if (!text.includes('\u001b')) {
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
                parts.push(<span key={key++} style={{ ...currentStyles }}>{segment}</span>);
            } else {
                parts.push(segment);
            }
        }

        // Update cumulative styles based on the SGR codes
        const codes = codeStr.split(';');
        for (const code of codes) {
            if (code === '0' || code === '') {
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
            parts.push(<span key={key++} style={{ ...currentStyles }}>{segment}</span>);
        } else {
            parts.push(segment);
        }
    }

    return <span>{parts}</span>;
}
