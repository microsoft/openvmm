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
