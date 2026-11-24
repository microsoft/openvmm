// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import assert from "node:assert/strict";
import { afterEach, mock, test } from "node:test";
import { fetchRunManifest } from "../src/utils/fetch_run_manifest.ts";

const run = "123_2";
const timestamp = "2026-10-30T16:00:00Z";
const valid = {
  version: 1,
  runNumber: run,
  creationTime: timestamp,
  tests: [
    { name: "x64/job/nested/passed", status: "passed" },
    { name: "aarch64/failed", status: "failed" },
  ],
};

afterEach(() => mock.restoreAll());

function respond(body, status = 200) {
  return mock.method(
    globalThis,
    "fetch",
    async () =>
      new Response(typeof body === "string" ? body : JSON.stringify(body), {
        status,
      }),
  );
}

test("reads a versioned manifest, reconstructs paths, and sorts like listings", async () => {
  const fetch = respond(valid);
  const signal = new AbortController().signal;
  assert.deepEqual(await fetchRunManifest(run, signal), {
    runNumber: run,
    creationTime: new Date(timestamp),
    tests: [
      {
        name: "aarch64/failed",
        status: "failed",
        path: `${run}/aarch64/failed`,
      },
      {
        name: "x64/job/nested/passed",
        status: "passed",
        path: `${run}/x64/job/nested/passed`,
      },
    ],
  });
  assert.deepEqual(fetch.mock.calls[0].arguments, [
    `https://openvmmghtestresults.blob.core.windows.net/results/attempts/${run}`,
    { signal },
  ]);
});

test("only missing or empty legacy markers request listing fallback", async () => {
  for (const [body, status] of [
    ["", 200],
    [" \n", 200],
    ["missing", 404],
  ]) {
    respond(body, status);
    assert.equal(await fetchRunManifest(run), undefined);
    mock.restoreAll();
  }
  respond({ ...valid, tests: [] });
  assert.deepEqual((await fetchRunManifest(run)).tests, []);
});

test("HTTP errors, malformed JSON, and invalid manifests fail explicitly", async () => {
  for (const status of [403, 429, 500]) {
    respond("error", status);
    await assert.rejects(
      fetchRunManifest(run),
      new RegExp(`manifest: ${status}`),
    );
    mock.restoreAll();
  }
  for (const body of [
    "not JSON",
    null,
    [],
    {},
    { ...valid, version: 2 },
    { ...valid, runNumber: "different" },
    { ...valid, creationTime: "invalid" },
    { ...valid, tests: {} },
    { ...valid, tests: [null] },
    { ...valid, tests: [{ name: "x64/test", status: "unknown" }] },
    ...[
      "test",
      "/x64/test",
      "x64//test",
      "x64/../test",
      "x64/./test",
      "x64/test\\file",
      "x64/test\nfile",
    ].map((name) => ({ ...valid, tests: [{ name, status: "passed" }] })),
    { ...valid, tests: [valid.tests[0], valid.tests[0]] },
  ]) {
    respond(body);
    await assert.rejects(
      fetchRunManifest(run),
      undefined,
      JSON.stringify(body),
    );
    mock.restoreAll();
  }
});

test("cancellation during manifest download is propagated, not treated as legacy", async () => {
  const controller = new AbortController();
  mock.method(globalThis, "fetch", async (_url, { signal }) => {
    assert.equal(signal, controller.signal);
    return {
      ok: true,
      async text() {
        controller.abort();
        return JSON.stringify(valid);
      },
    };
  });
  await assert.rejects(fetchRunManifest(run, controller.signal), {
    name: "AbortError",
  });
});
