// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Data types used across the app
export interface RunData {
  name: string;
  creationTime: Date;
  lastModified: Date;
  etag: string;
  contentLength: number;
  metadata: RunMetadata;
}

export interface RunMetadata {
  petriFailed: number;
  petriPassed: number;
  ghBranch: string;
  ghPr?: string;
  prTitle?: string;
}

// A parsed run identifier. Run identifiers are stored as "<runId>_<attempt>"
// (e.g. "16234567890_1"), where runId is the GitHub Actions run id and attempt
// is the re-run attempt number (the first attempt is "1").
export interface ParsedRunKey {
  runId: string;
  attempt: string; // empty string if the key has no attempt suffix
}

/**
 * Split a "<runId>_<attempt>" run key into its parts for display / linking.
 * Falls back gracefully (attempt = "") for keys without an attempt suffix.
 * Uses the last underscore so a numeric runId is split unambiguously.
 */
export function parseRunKey(runKey: string): ParsedRunKey {
  const underscore = runKey.lastIndexOf("_");
  if (underscore === -1) {
    return { runId: runKey, attempt: "" };
  }
  return {
    runId: runKey.slice(0, underscore),
    attempt: runKey.slice(underscore + 1),
  };
}

export interface TestResult {
  name: string;
  status: "passed" | "failed";
  path: string;
  duration?: number;
}

export interface RunDetailsData {
  creationTime?: Date;
  runNumber: string;
  tests: TestResult[];
}

// Mapping of PR number (as string) -> PR title
export type PullRequestTitles = Record<string, string>;


export interface TestRunInfo {
    runNumber: string;
    creationTime?: Date;
    status: 'passed' | 'failed' | 'unknown';
}

export interface TestData {
    architecture: string;
    name: string;
    failedCount: number;
    totalCount: number;
}

export interface LogEntry {
  index: number;
  timestamp: string;
  relative: string;
  severity: string;
  source: string;
  logMessage: LogMessage; // message with attachment links
  screenshot: string | null;
}

export interface LogMessage {
  message: string;      // Plain text (ANSI stripped) — used for search/filter/copy
  rawMessage: string;   // Original message with ANSI escape codes — used for colored rendering
  link_string: string;  // This is a space-separated string of link texts for searching/sorting
  links: LogLink[];
}

export interface LogLink {
  text: string;
  url: string;
  inspect: boolean;
}

// Concurrency settings when fetching test results
export const CONCURRENCY_FOREGROUND = 15;
export const CONCURRENCY_BACKGROUND = 5;

export type InspectPrimitive =
  | { type: "string"; value: string }
  | { type: "bytes"; value: string }
  | { type: "unevaluated" }
  | { type: "boolean"; value: boolean }
  | { type: "error"; value: string }
  | { type: "number"; value: string };

export interface InspectObject {
  type: "object";
  children: { key: string; value: InspectNode }[];
}
export type InspectNode = InspectPrimitive | InspectObject;
