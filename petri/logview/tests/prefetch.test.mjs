// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import assert from "node:assert/strict";
import { afterEach, beforeEach, test, mock } from "node:test";
import { setImmediate } from "node:timers/promises";
import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { PrefetchQueue, waitForIdle } from "../src/utils/prefetch.ts";

let client;
let queue;
let stop;
let idle;
let nextIdleId;
const originalRequestIdleCallback = globalThis.requestIdleCallback;
const originalCancelIdleCallback = globalThis.cancelIdleCallback;

beforeEach(() => {
  mock.timers.enable({ apis: ["setTimeout"] });
  idle = new Map();
  nextIdleId = 0;
  globalThis.requestIdleCallback = (callback) => {
    idle.set(++nextIdleId, callback);
    return nextIdleId;
  };
  globalThis.cancelIdleCallback = (id) => idle.delete(id);
  client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: Infinity } },
  });
  queue = new PrefetchQueue(client);
  stop = queue.start();
});

afterEach(async () => {
  stop();
  await setImmediate();
  client.clear();
  mock.timers.reset();
  mock.restoreAll();
  if (originalRequestIdleCallback) {
    globalThis.requestIdleCallback = originalRequestIdleCallback;
    globalThis.cancelIdleCallback = originalCancelIdleCallback;
  } else {
    delete globalThis.requestIdleCallback;
    delete globalThis.cancelIdleCallback;
  }
});

function runIdle(milliseconds = 500) {
  mock.timers.tick(milliseconds);
  const callbacks = [...idle.values()];
  idle.clear();
  for (const callback of callbacks) callback();
}

function task(name) {
  const attempts = [];
  return {
    attempts,
    queryKey: [name],
    queryFn(signal) {
      const deferred = Promise.withResolvers();
      attempts.push({ ...deferred, signal });
      signal.addEventListener("abort", () => deferred.reject(signal.reason), {
        once: true,
      });
      return deferred.promise;
    },
  };
}

test("waits for a quiet period and an idle callback, with only one warm query", async () => {
  const first = task("first");
  const second = task("second");
  queue.add(first);
  queue.add(second);
  mock.timers.tick(499);
  assert.equal(idle.size, 0);
  mock.timers.tick(1);
  assert.equal(first.attempts.length, 0);
  assert.equal(idle.size, 1);
  runIdle(0);
  assert.equal(first.attempts.length, 1);
  runIdle(5000);
  assert.equal(second.attempts.length, 0);
  first.attempts[0].resolve("cached");
  await setImmediate();
  runIdle();
  assert.equal(second.attempts.length, 1);
  queue.add(first);
  second.attempts[0].resolve("cached too");
  await setImmediate();
  runIdle();
  assert.equal(first.attempts.length, 1, "Successful warming is cached");
});

test("does not start warming while foreground data is loading", async () => {
  const visible = Promise.withResolvers();
  const foreground = client.fetchQuery({
    queryKey: ["visible"],
    queryFn: () => visible.promise,
  });
  const background = task("background");
  queue.add(background);
  runIdle(10000);
  assert.equal(background.attempts.length, 0);
  visible.resolve("visible");
  await foreground;
  runIdle();
  assert.equal(background.attempts.length, 1);
});

test("aborts speculative work for foreground loading and resumes it afterward", async () => {
  const background = task("background");
  queue.add(background);
  runIdle();
  const visible = Promise.withResolvers();
  const foreground = client.fetchQuery({
    queryKey: ["visible"],
    queryFn: () => visible.promise,
  });
  assert.equal(background.attempts[0].signal.aborted, true);
  await setImmediate();
  runIdle(10000);
  assert.equal(background.attempts.length, 1);
  visible.resolve("visible");
  await foreground;
  runIdle();
  assert.equal(background.attempts.length, 2);
  background.attempts[1].resolve("warm");
});

test("a foreground observer adopts matching work across route cleanup", async () => {
  const background = task("same-log");
  queue.add(background);
  runIdle();
  const observer = new QueryObserver(client, {
    queryKey: background.queryKey,
    queryFn: () => assert.fail("Must not duplicate the in-flight log request"),
    staleTime: Infinity,
  });
  const unsubscribe = observer.subscribe(() => {});
  stop();
  await setImmediate();
  assert.equal(background.attempts[0].signal.aborted, false);
  background.attempts[0].resolve("ready");
  await setImmediate();
  assert.equal(observer.getCurrentResult().data, "ready");
  assert.equal(background.attempts.length, 1);
  unsubscribe();
});

test("foreground adoption releases idle processing after route cleanup", async () => {
  const queryKey = ["waiting-log"];
  queue.add({
    queryKey,
    queryFn: async (signal, foregroundSignal) => {
      await waitForIdle(signal, foregroundSignal);
      return "processed";
    },
  });
  runIdle();
  assert.equal(
    idle.size,
    1,
    "Background processing waits for another idle slot",
  );
  stop();

  const observer = new QueryObserver(client, {
    queryKey,
    queryFn: () => assert.fail("Must adopt the waiting log"),
    staleTime: Infinity,
  });
  const unsubscribe = observer.subscribe(() => {});
  await setImmediate();
  assert.equal(observer.getCurrentResult().data, "processed");
  assert.equal(idle.size, 0, "Promotion cancels the pending idle callback");
  unsubscribe();
});

test("a log adopted during download bypasses its later idle wait", async () => {
  const downloaded = Promise.withResolvers();
  const queryKey = ["downloading-log"];
  queue.add({
    queryKey,
    queryFn: async (signal, foregroundSignal) => {
      await downloaded.promise;
      await waitForIdle(signal, foregroundSignal);
      return "processed";
    },
  });
  runIdle();

  const observer = new QueryObserver(client, {
    queryKey,
    queryFn: () => assert.fail("Must share the download"),
    staleTime: Infinity,
  });
  const unsubscribe = observer.subscribe(() => {});
  downloaded.resolve();
  await setImmediate();
  assert.equal(observer.getCurrentResult().data, "processed");
  assert.equal(idle.size, 0);
  unsubscribe();
});

test("hover promotion preempts unrelated warming and goes ahead of queued work", async () => {
  const first = task("first");
  const second = task("second");
  const hovered = task("hovered");
  queue.add(first);
  queue.add(second);
  queue.add(hovered);
  runIdle();
  queue.add(hovered, true);
  assert.equal(first.attempts[0].signal.aborted, true);
  await setImmediate();
  runIdle(0);
  assert.equal(hovered.attempts.length, 1);
  assert.equal(second.attempts.length, 0);
  hovered.attempts[0].resolve("hovered");
  await setImmediate();
  runIdle();
  assert.equal(second.attempts.length, 1);
});

test("route cleanup cancels queued and unobserved active work", async () => {
  const first = task("first");
  const second = task("second");
  queue.add(first);
  queue.add(second);
  runIdle();
  stop();
  await setImmediate();
  assert.equal(first.attempts[0].signal.aborted, true);
  runIdle(10000);
  assert.equal(second.attempts.length, 0);
});

test("failed warming is reported once, then the queue continues", async () => {
  const warning = mock.method(console, "warn", () => {});
  const first = task("broken");
  const second = task("next");
  queue.add(first);
  queue.add(second);
  runIdle();
  first.attempts[0].reject(new Error("fixture failure"));
  await setImmediate();
  assert.equal(warning.mock.callCount(), 1);
  runIdle();
  assert.equal(second.attempts.length, 1);
  assert.equal(first.attempts.length, 1, "Speculative failures do not retry");
});

test("idle processing is cancellable before parsing starts", async () => {
  const controller = new AbortController();
  const waiting = waitForIdle(controller.signal);
  assert.equal(idle.size, 1);
  controller.abort();
  await assert.rejects(waiting, { name: "AbortError" });
  assert.equal(idle.size, 0);
});

test("warming also works without requestIdleCallback support", () => {
  delete globalThis.requestIdleCallback;
  const background = task("fallback");
  queue.add(background);
  mock.timers.tick(500);
  mock.timers.tick(0);
  assert.equal(background.attempts.length, 1);
});
