// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { useEffect, useMemo } from "react";
import {
  hashKey,
  isCancelledError,
  useQueryClient,
  type QueryClient,
} from "@tanstack/react-query";

function runWhenIdle(callback: () => void): () => void {
  if (typeof requestIdleCallback === "function") {
    const id = requestIdleCallback(callback);
    return () => cancelIdleCallback(id);
  }
  const id = setTimeout(callback, 0);
  return () => clearTimeout(id);
}

export function waitForIdle(signal?: AbortSignal): Promise<void> {
  signal?.throwIfAborted();
  return new Promise((resolve, reject) => {
    const cancel = runWhenIdle(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    });
    function onAbort() {
      cancel();
      reject(signal?.reason);
    }
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

interface PrefetchTask {
  queryKey: readonly string[];
  queryFn: (signal: AbortSignal) => Promise<unknown>;
}

interface QueuedTask extends PrefetchTask {
  priority: boolean;
}

export class PrefetchQueue {
  private client: QueryClient;
  private pending = new Map<string, QueuedTask>();
  private active?: { hash: string; task: QueuedTask; cancelled: boolean };
  private cancelScheduled?: () => void;
  private running = false;

  constructor(client: QueryClient) {
    this.client = client;
  }

  start(): () => void {
    this.running = true;
    const unsubscribe = this.client.getQueryCache().subscribe(() => {
      if (this.hasForegroundFetch()) {
        this.clearScheduled();
        this.cancelActive(true);
      } else {
        this.schedule();
      }
    });
    this.schedule();
    return () => {
      this.running = false;
      unsubscribe();
      this.clearScheduled();
      this.pending.clear();
      // Let a newly mounted route adopt the same in-flight query first.
      queueMicrotask(() => {
        if (!this.running) this.cancelActive(false);
      });
    };
  }

  add(task: PrefetchTask, priority = false): void {
    const hash = hashKey(task.queryKey);
    if (
      this.client.getQueryData(task.queryKey) !== undefined ||
      this.active?.hash === hash
    ) {
      return;
    }
    if (priority) {
      this.pending.delete(hash);
      this.pending = new Map([[hash, { ...task, priority }], ...this.pending]);
      this.clearScheduled();
      this.cancelActive(true);
    } else if (!this.pending.has(hash)) {
      this.pending.set(hash, { ...task, priority });
    }
    this.schedule();
  }

  private hasForegroundFetch(): boolean {
    return this.client
      .getQueryCache()
      .findAll({ fetchStatus: "fetching" })
      .some((query) => query.queryHash !== this.active?.hash);
  }

  private clearScheduled(): void {
    this.cancelScheduled?.();
    this.cancelScheduled = undefined;
  }

  private cancelActive(requeue: boolean): void {
    const active = this.active;
    if (!active || active.cancelled) return;
    const query = this.client.getQueryCache().find({
      queryKey: active.task.queryKey,
      exact: true,
    });
    if (query && query.getObserversCount() > 0) return;
    active.cancelled = true;
    if (requeue) this.pending.set(active.hash, active.task);
    void this.client.cancelQueries({
      queryKey: active.task.queryKey,
      exact: true,
    });
  }

  private schedule(): void {
    if (
      !this.running ||
      this.active ||
      this.cancelScheduled ||
      this.pending.size === 0 ||
      this.hasForegroundFetch()
    ) {
      return;
    }
    const next = this.pending.values().next().value;
    const id = setTimeout(
      () => {
        this.cancelScheduled = runWhenIdle(() => {
          this.cancelScheduled = undefined;
          this.runNext();
        });
      },
      next?.priority ? 0 : 500,
    );
    this.cancelScheduled = () => clearTimeout(id);
  }

  private runNext(): void {
    if (!this.running || this.active || this.hasForegroundFetch()) return;
    const next = this.pending.entries().next().value;
    if (!next) return;
    const [hash, task] = next;
    this.pending.delete(hash);
    this.active = { hash, task, cancelled: false };
    void this.client
      .fetchQuery({
        queryKey: task.queryKey,
        queryFn: ({ signal }) => task.queryFn(signal),
        staleTime: Infinity,
        gcTime: Infinity,
        retry: false,
      })
      .catch((error: unknown) => {
        if (!isCancelledError(error)) {
          console.warn("Background prefetch failed:", task.queryKey, error);
        }
      })
      .finally(() => {
        this.active = undefined;
        this.schedule();
      });
  }
}

export function usePrefetchQueue(scope: string): PrefetchQueue {
  const client = useQueryClient();
  const queue = useMemo(() => new PrefetchQueue(client), [client, scope]);
  useEffect(() => queue.start(), [queue]);
  return queue;
}
