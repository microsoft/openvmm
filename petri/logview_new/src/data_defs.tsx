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
