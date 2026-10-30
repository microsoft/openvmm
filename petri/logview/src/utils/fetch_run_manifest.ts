// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import type { RunDetailsData, TestResult } from "../data_defs";

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export async function fetchRunManifest(
  runNumber: string,
  signal?: AbortSignal,
): Promise<RunDetailsData | undefined> {
  const response = await fetch(
    `https://openvmmghtestresults.blob.core.windows.net/results/attempts/${encodeURIComponent(runNumber)}`,
    { signal },
  );
  // Older runs either have an empty attempt marker or no marker at all.
  if (response.status === 404) return undefined;
  if (!response.ok) {
    throw new Error(
      `Failed to fetch run manifest: ${response.status} ${response.statusText}`,
    );
  }
  const text = await response.text();
  signal?.throwIfAborted();
  if (text.trim() === "") return undefined;

  const manifest: unknown = JSON.parse(text);
  if (!isObject(manifest) || manifest.version !== 1) {
    throw new Error(`Unsupported run manifest version for ${runNumber}`);
  }
  if (
    manifest.runNumber !== runNumber ||
    typeof manifest.creationTime !== "string" ||
    !Array.isArray(manifest.tests)
  ) {
    throw new Error(`Invalid run manifest for ${runNumber}`);
  }
  const creationTime = new Date(manifest.creationTime);
  if (Number.isNaN(creationTime.getTime())) {
    throw new Error(`Invalid run manifest creation time for ${runNumber}`);
  }

  const tests: TestResult[] = [];
  const names = new Set<string>();
  for (const test of manifest.tests) {
    if (
      !isObject(test) ||
      typeof test.name !== "string" ||
      (test.status !== "passed" && test.status !== "failed")
    ) {
      throw new Error(`Invalid run manifest test for ${runNumber}`);
    }
    const parts = test.name.split("/");
    if (
      parts.length < 2 ||
      parts.some((part) => part === "" || part === "." || part === "..") ||
      /[\\\u0000-\u001f]/.test(test.name) ||
      names.has(test.name)
    ) {
      throw new Error(
        `Invalid or duplicate run manifest test path: ${test.name}`,
      );
    }
    names.add(test.name);
    tests.push({
      name: test.name,
      status: test.status,
      path: `${runNumber}/${test.name}`,
    });
  }
  tests.sort((a, b) => a.name.localeCompare(b.name));
  return { runNumber, creationTime, tests };
}
