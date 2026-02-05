// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

export interface Shortcut {
  shortcutParts: string[];
  descriptions: string[];
}

export interface ShortcutCategory {
  title: string;
  note: string;
  shortcuts: readonly Shortcut[];
}

export const shortcutSections: readonly ShortcutCategory[] = [
  {
    title: "Search",
    note: "Applies anywhere the search bar is present (Runs, Tests, Log Viewer header, Inspect overlay).",
    shortcuts: [
      {
        shortcutParts: ["Ctrl", "F"],
        descriptions: ["Focuses the search bar and selects any existing text."],
      },
      {
        shortcutParts: ["Esc"],
        descriptions: [
          "When the search bar has text: clears it.",
          "When the search bar is empty and focused: removes focus.",
        ],
      },
      {
        shortcutParts: ["<col_name>:<search_term>"],
        descriptions: [
          "Logviewer supports column-scoped filtering with prefixes including `severity:`, `source:`, and `message:`.",
          "Multiple tokens are ANDed together.",
        ],
      },
      {
        shortcutParts: ["<search_term> <search_term> ..."],
        descriptions: [
          "Searches several terms at once ANDing the results together.",
          "Supports searching several terms with column filters like: 'severity:error message:panic' [Where column filters are supported]",
        ],
      },
    ],
  },
  {
    title: "Log Viewer",
    note: "These shortcuts only apply when a log row is selected (click a row to select). They do not override native copy when you have selected text.",
    shortcuts: [
      {
        shortcutParts: ["Ctrl", "C"],
        descriptions: [
          "Copies a plain-text block for the selected log line (timestamp, relative, severity, source, message, and screenshot URL if present).",
        ],
      },
      {
        shortcutParts: ["Ctrl", "Shift", "C"],
        descriptions: [
          "Copies a deep link URL to the selected log line (adds/updates `log=` in the hash-router query string).",
        ],
      },
    ],
  },
  {
    title: "Inspect Viewer",
    note: "These apply to the Inspect overlay in parsed/tree mode (not raw mode). Arrow-key navigation is disabled while typing in inputs.",
    shortcuts: [
      {
        shortcutParts: ["Ctrl", "C"],
        descriptions: [
          "Copies the selected node/subtree as formatted text.",
          "If you have highlighted text, native copy behavior is used instead.",
          "If there is a search filter active, it still copies all the nodes.",
        ],
      },
      {
        shortcutParts: ["↑ / ↓"],
        descriptions: ["Moves selection to the previous/next visible tree row."],
      },
      {
        shortcutParts: ["→ / ←"],
        descriptions: ["Expands/collapses the selected node."],
      },
      {
        shortcutParts: ["+ / ="],
        descriptions: ["Expands all descendants of the selected node."],
      },
      {
        shortcutParts: ["-"],
        descriptions: ["Collapses all descendants of the selected node."],
      },
    ],
  },
];
