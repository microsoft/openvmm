// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { LogEntry } from "../data_defs";

/**
 * Fetch the raw petri.jsonl log content for a given run / architecture / test path.
 * Path layout (simplified, no job dimension):
 *   runs are stored under: <runId>/<architecture?>/<testNameRemainder>/petri.jsonl
 * If architecture is empty/undefined we omit that path element.
 * Returns the resolved URL and raw text (may be empty string if file exists but is blank).
 */
export async function fetchLog(
  runId: string,
  architecture: string | undefined,
  testNameRemainder: string
): Promise<{ url: string; text: string }> {
  if (!runId) throw new Error("runId required");
  const parts: string[] = [runId];
  if (architecture) parts.push(architecture);
  if (testNameRemainder) parts.push(testNameRemainder);
  const url = `https://openvmmghtestresults.blob.core.windows.net/results/${parts.join("/")}/petri.jsonl`;
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(
      `Failed to fetch log (${response.status} ${response.statusText}) for ${url}`
    );
  }
  const text = await response.text();
  return { url, text };
}

// --------------------------------------------
// petri.jsonl parsing (raw records only)
// --------------------------------------------

export interface RawLogRecord {
  timestamp: string;
  message?: string;
  severity?: string;
  source?: string;
  attachment?: string;
  // Allow arbitrary extra properties without losing information
  [key: string]: any;
}

/**
 * Parse a petri.jsonl file (newline-delimited JSON objects) into an array of raw records.
 * - Trims empty lines
 * - JSON parses each line
 * - Sorts ascending by timestamp (stable)
 * Throws on first parse error to surface corrupt data quickly.
 */
export function parseLogText(text: string): RawLogRecord[] {
  if (!text) return [];
  const lines = text
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l.length > 0);

  const records: RawLogRecord[] = [];
  for (let i = 0; i < lines.length; i++) {
    const raw = lines[i];
    try {
      const obj = JSON.parse(raw);
      records.push(obj as RawLogRecord);
    } catch (e) {
      console.error(`Failed to parse log`);
    }
  }

  records.sort((a, b) => {
    const ta = new Date(a.timestamp || 0).getTime();
    const tb = new Date(b.timestamp || 0).getTime();
    return ta - tb;
  });
  return records;
}

// --------------------------------------------
// Processed petri log entries (UI friendly)
// --------------------------------------------

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function removeTimestampPrefix(orig: string, entryTimestamp: Date): string {
  const message = orig.trim();
  const i = message.indexOf(" ");
  if (i === -1) return orig;
  let ts = message.slice(0, i);
  if (ts.endsWith("s")) {
    // relative like 12.345s
    const secs = parseFloat(ts.slice(0, -1));
    if (!isNaN(secs)) return message.slice(i + 1);
  }
  if (ts.startsWith("[")) ts = ts.slice(1, -1);
  const parsedTs = new Date(ts);
  if (isNaN(parsedTs.getTime())) return orig;
  parsedTs.setMilliseconds(0);
  const truncated = new Date(entryTimestamp.getTime());
  truncated.setMilliseconds(0);
  if (parsedTs.getTime() !== truncated.getTime()) return orig;
  return message.slice(i + 1);
}

function extractSeverity(
  orig: string,
  defaultSeverity: string
): { message: string; severity: string } {
  const severityLevels = ["ERROR", "WARN", "INFO", "DEBUG"];
  const trimmed = orig.trim();
  for (const lvl of severityLevels) {
    if (trimmed.startsWith(lvl)) {
      return { message: trimmed.slice(lvl.length + 1), severity: lvl };
    }
  }
  return { message: orig, severity: defaultSeverity };
}

function formatRelative(start: string, current: string): string {
  const deltaMs = new Date(current).getTime() - new Date(start).getTime();
  const sec = ((deltaMs / 1000) % 60).toFixed(3);
  const min = Math.floor((deltaMs / 60000) % 60);
  const hr = Math.floor(deltaMs / 3600000);
  return `${hr > 0 ? hr + "h " : ""}${min}m ${sec}s`;
}

// Map ANSI SGR codes to inline styles (subset used in original UI)
const ANSI_STYLE_MAP: Record<string, string> = {
  "1": "font-weight:bold",
  "3": "font-style:italic",
  "4": "text-decoration:underline",
  "30": "color:black",
  "31": "color:red",
  "32": "color:green",
  "33": "color:#b58900",
  "34": "color:blue",
  "35": "color:magenta",
  "36": "color:cyan",
  "37": "color:white",
  "90": "color:gray",
  "91": "color:lightcoral",
  "92": "color:lightgreen",
  "93": "color:gold",
  "94": "color:lightskyblue",
  "95": "color:plum",
  "96": "color:lightcyan",
  "97": "color:white",
  "39": "color:inherit",
};

function ansiToHtml(str: string): string {
  const ESC_REGEX = /\u001b\[([0-9;]*)m/g;
  let html = "";
  let lastIndex = 0;
  let current: string[] = [];
  const flush = (text: string) => {
    if (!text) return;
    const esc = escapeHtml(text);
    if (current.length) {
      html += `<span style="${current.join(";")}">${esc}</span>`;
    } else {
      html += esc;
    }
  };
  for (const match of str.matchAll(ESC_REGEX)) {
    const [full, codesStr] = match;
    const idx = match.index || 0;
    flush(str.slice(lastIndex, idx));
    const codes = codesStr.split(";").filter((c) => c.length > 0);
    for (const code of codes) {
      if (code === "0") {
        current = [];
        continue;
      }
      const style = ANSI_STYLE_MAP[code];
      if (style) {
        const prop = style.split(":")[0];
        current = current.filter((s) => !s.startsWith(prop));
        current.push(style);
      }
    }
    lastIndex = idx + full.length;
  }
  flush(str.slice(lastIndex));
  return html;
}

/**
 * High-level fetch + process for LogViewer. Produces display-ready entries.
 */
export async function fetchProcessedLog(
  runId: string,
  architecture: string | undefined,
  testNameRemainder: string
): Promise<LogEntry[]> {
  const { url, text } = await fetchLog(
    runId,
    architecture,
    testNameRemainder
  );
  if (!text) return [];
  const raw = parseLogText(text);
  const entries: LogEntry[] = [];
  let start: string | null = null;
  for (let i = 0; i < raw.length; i++) {
    const rec = raw[i];
    const timestamp = rec.timestamp;
    if (!start) start = timestamp;
    let message = rec.message || "";
    let severity = rec.severity || "INFO";
    const source = rec.source || (rec.attachment ? "attachment" : "unknown");

    message = removeTimestampPrefix(message, new Date(timestamp));
    const sevExtract = extractSeverity(message, severity);
    message = sevExtract.message;
    severity = sevExtract.severity;

    let messageHtml = ansiToHtml(message);
    let screenshot: string | null = null;
    if (rec.attachment) {
      const attachmentUrl = new URL(rec.attachment, url).toString();
      // Only treat PNGs as screenshots if they're NOT inspect files
      if (
        rec.attachment.endsWith(".png") &&
        !rec.attachment.includes("inspect") &&
        entries.length > 0
      ) {
        // associate with previous entry
        entries[entries.length - 1].screenshot = attachmentUrl;
        continue; // don't emit separate row
      }
      // Inspect attachment gets two links (inspect + raw); others single link
      if (rec.attachment.includes("inspect")) {
        messageHtml +=
          (messageHtml ? " " : "") +
          `<a href="${attachmentUrl}" class="attachment" target="_blank" data-inspect="true">${escapeHtml(rec.attachment)}</a> <a href="${attachmentUrl}" class="attachment" target="_blank">[raw]</a>`;
      } else {
        messageHtml +=
          (messageHtml ? " " : "") +
          `<a href="${attachmentUrl}" class="attachment" target="_blank">${escapeHtml(rec.attachment)}</a>`;
      }
    }

    entries.push({
      index: i,
      timestamp,
      relative: start ? formatRelative(start, timestamp) : "0m 0.000s",
      severity,
      source,
      messageHtml,
      messageText: messageHtml.replace(/<[^>]+>/g, "").toLowerCase(),
      screenshot,
    });
  }
  return entries;
}