// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import assert from "node:assert/strict";
import { after, afterEach, before, beforeEach, mock, test } from "node:test";
import { fileURLToPath } from "node:url";
import { QueryClient } from "@tanstack/react-query";
import { createServer } from "vite";

let server;
let fetchTestAnalysis;
let client;
let fetchRuns;
const originalDOMParser = globalThis.DOMParser;

before(async () => {
  server = await createServer({
    root: fileURLToPath(new URL("..", import.meta.url)),
    configFile: false,
    server: { middlewareMode: true, ws: false },
    appType: "custom",
  });
  ({ fetchTestAnalysis } = await server.ssrLoadModule(
    "/src/utils/fetch_runs_data.ts",
  ));
});

after(async () => {
  await server?.close();
});

beforeEach(() => {
  client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity } },
  });
  fetchRuns = mock.method(globalThis, "fetch", async () => new Response(""));
  globalThis.DOMParser = class {
    parseFromString() {
      return { getElementsByTagName: () => [] };
    }
  };
});

afterEach(() => {
  client.clear();
  mock.restoreAll();
  if (originalDOMParser) {
    globalThis.DOMParser = originalDOMParser;
  } else {
    delete globalThis.DOMParser;
  }
});

test("test analysis refreshes stale runs before using them", async () => {
  const updatedAt = Date.now() - 3 * 60 * 1000;
  client.setQueryData(["runs"], [], { updatedAt });

  await fetchTestAnalysis("main", client);

  assert.equal(fetchRuns.mock.callCount(), 1);
  assert.ok(client.getQueryState(["runs"]).dataUpdatedAt > updatedAt);
});

test("test analysis reuses fresh runs without another download", async () => {
  client.setQueryData(["runs"], []);

  await fetchTestAnalysis("main", client);

  assert.equal(fetchRuns.mock.callCount(), 0);
});
