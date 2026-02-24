# Virtio-blk Implementation Plan

## Problem Statement

Add a virtio-blk block device implementation to OpenVMM. The device should:
- Follow the OASIS VIRTIO v1.2 spec (Section 5.2) for the block device
- Support all the same disk backends as NVMe and Storvsp (file, VHD, ramdisk, layered, etc.)
- Be exposed as a PCI device via the existing virtio PCI transport
- Be testable via the petri integration test framework
- Follow the existing codebase patterns (VirtioDevice trait, resource resolution, etc.)

## Approach

Model the implementation after `virtio_net` (for the VirtioDevice trait pattern) and `nvme` (for
disk backend integration). The device will:
1. Implement the `VirtioDevice` trait directly (modern pattern)
2. Use `disk_backend::Disk` via `Resource<DiskHandleKind>` for disk backends
3. Translate virtio descriptor chains into `RequestBuffers` for disk I/O
4. Be wrapped in `VirtioPciDeviceHandle` for PCI transport

## Workplan

### Phase 1: Core Device Implementation

- [x] **1.1 Create `virtio_blk` crate skeleton**
- [x] **1.2 Define virtio-blk spec constants (`spec.rs`)**
- [x] **1.3 Implement the `VirtioDevice` trait (`lib.rs`)**
- [x] **1.4 Implement request processing worker**
- [x] **1.5 Descriptor-to-RequestBuffers translation**

### Phase 2: Resource Plumbing

- [x] **2.1 Add resource handle to `virtio_resources`**
- [x] **2.2 Create resolver (`resolver.rs`)**
- [x] **2.3 Register resolver in `openvmm_resources`**

### Phase 3: CLI/Configuration Integration

- [x] **3.1 Add VirtioBlk to `DiskLocation` enum and StorageBuilder**
- [x] **3.2 Add `--virtio-blk` CLI argument**

### Phase 4: Petri Test Integration

- [x] **4.1 Add VirtioBlk to petri storage types**
- [x] **4.2 Write integration test (`virtio_blk_device`)**

### Phase 5: Build & Verification

- [x] **5.1 All affected crates build cleanly**
- [x] **5.2 `cargo xtask fmt --fix` passes**
- [x] **5.3 `cargo doc` passes**

### Phase 6: Concurrent IO

- [x] **6.1 Concurrent IO dispatch** (done — `FuturesUnordered` + `poll_fn` loop)

### Phase 7: In-flight IO Lifecycle

  **Problem**: The `FuturesUnordered` that tracks in-flight IOs is a local variable
  inside `WorkerTask::run()`. When `TaskControl::stop()` is called,
  `stop.until_stopped()` drops its inner future, which drops the
  `FuturesUnordered`, which drops every in-flight IO future mid-execution.

  Each dropped future holds a `VirtioQueueCallbackWork` whose `Drop` impl
  auto-completes the descriptor with 0 bytes written (returning it to the guest
  as an empty/failed response). So descriptors are never leaked — but:

  1. **IOs silently fail.** A read or write that was about to finish is discarded
     and the guest sees a zero-length completion. The guest driver will likely
     treat this as an error and may retry, but data could be lost for writes
     that were partially committed to the backend.

  2. **IOs don't survive stop/start.** `TaskControl` is designed to let state
     persist across `stop()` → `start()` cycles (e.g., for save/restore,
     config changes, or live resize). Because the `FuturesUnordered` is a
     local, all in-flight work is lost on every stop — even when a restart
     immediately follows.

  3. **No drain on disable.** `VirtioBlkDevice::disable()` spawns a detached
     future that calls `worker.task.stop().await` and then drops everything.
     It never waits for in-flight IOs to finish. The silently-failed
     completions race with the device teardown.

  NVMe and Storvsp both solve this the same way: the `FuturesUnordered` lives
  in the persistent `TaskControl` state `S`, and an explicit `drain()` method
  runs all remaining futures to completion after the task is stopped.

- [ ] **7.1 Move `FuturesUnordered` into `WorkerState`**

  Move the `ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoCompletion> + Send>>>`
  field from a local variable inside `run()` into the `WorkerState` struct
  (the `S` parameter of `TaskControl<WorkerTask, WorkerState>`).

  Because `WorkerState` persists across `stop()`/`start()` cycles, in-flight
  IO futures will survive a task stop and resume execution on the next `start()`.

  **Changes to `WorkerState`**:
  ```rust
  struct WorkerState {
      disk: Disk,
      memory: GuestMemory,
      read_only: bool,
      stats: WorkerStats,
      ios: FuturesUnordered<Pin<Box<dyn Future<Output = IoCompletion> + Send>>>,
  }
  ```

  The `#[inspect(skip)]` attribute should be added for `ios` since
  `FuturesUnordered` doesn't implement `Inspect`. Alternatively, inspect it
  via its length: `#[inspect(with = "FuturesUnordered::len")]`.

  **Changes to `AsyncRun::run()`**: Instead of creating a local
  `FuturesUnordered`, borrow `state.ios` in the poll loop:
  ```rust
  async fn run(&mut self, stop: &mut StopTask<'_>, state: &mut WorkerState) -> ... {
      stop.until_stopped(async {
          loop {
              let event = std::future::poll_fn(|cx| {
                  if let Poll::Ready(Some(completion)) = state.ios.poll_next_unpin(cx) {
                      return Poll::Ready(Event::Completed(completion));
                  }
                  if state.ios.len() < MAX_IO_DEPTH {
                      if let Poll::Ready(item) = self.queue.poll_next_unpin(cx) { ... }
                  }
                  Poll::Pending
              }).await;
              // ... handle event, push new futures into state.ios ...
          }
      }).await
  }
  ```

  `until_stopped` will drop the async block (which borrows `state.ios`), but
  the futures inside `state.ios` remain alive because the collection itself
  is owned by `WorkerState`.

  **Changes to `enable()`**: Initialize `ios: FuturesUnordered::new()` in the
  `WorkerState` passed to `task.insert(...)`.

- [ ] **7.2 Add `WorkerState::drain()` method**

  Add a drain method that runs all in-flight IO futures to completion,
  following the NVMe `IoState::drain()` pattern:
  ```rust
  impl WorkerState {
      /// Drain all in-flight IOs to completion.
      /// This future may be dropped and re-issued safely.
      async fn drain(&mut self) {
          while let Some(completion) = self.ios.next().await {
              match completion.stat {
                  IoStat::Read => self.stats.read_ops.increment(),
                  IoStat::Write => self.stats.write_ops.increment(),
                  IoStat::Flush => self.stats.flush_ops.increment(),
                  IoStat::Discard => self.stats.discard_ops.increment(),
                  IoStat::Error => self.stats.errors.increment(),
                  IoStat::None => {}
              }
          }
      }
  }
  ```

  This gives the disk backend a chance to complete writes and flushes rather
  than silently dropping them. The `process_request` function already writes
  the status byte and calls `work.complete()` before returning, so each
  drained future fully completes its guest-visible side effects.

- [ ] **7.3 Drain on disable (interim — `.detach()` with drain)**

  As an interim fix before Phase 8, update `disable()` to at least drain IOs
  within the detached task:
  ```rust
  fn disable(&mut self) {
      if let Some(mut worker) = self.worker.take() {
          self.driver.spawn("shutdown-virtio-blk", async move {
              worker.task.stop().await;
              if let Some(state) = worker.task.state_mut() {
                  state.drain().await;
              }
              worker.task.remove();
          }).detach();
      }
  }
  ```

  This is still fire-and-forget (the guest can observe status=0 before drain
  finishes), but it's strictly better than the current behavior since IOs
  will complete rather than being silently dropped. The proper fix comes in
  Phase 8.

- [ ] **7.4 Verify the poll-order concern**

  The current `poll_fn` polls `ios` first (to free slots), then polls the
  queue for new work. After moving `ios` to state, verify this ordering
  still works correctly — specifically that the borrow of `state.ios` inside
  the `poll_fn` closure is compatible with also pushing new futures into
  `state.ios` when handling `Event::NewWork`.

  The pattern should work because `poll_fn` returns one event at a time, and
  the push into `state.ios` happens *after* the `poll_fn` completes (in the
  match arm), not inside the closure. But this needs to be confirmed at
  compile time. If the borrow checker objects, the workaround is to accumulate
  new futures in a small local `Vec` inside the closure and drain them into
  `state.ios` after the poll.

  **Files changed**: `vm/devices/virtio/virtio_blk/src/lib.rs`

### Phase 8: Async Disable (Cross-cutting)

  **Problem**: `VirtioDevice::disable()` is sync, but proper device reset requires
  waiting for in-flight DMA to complete before signaling reset to the guest.
  All current virtio device implementations with async cleanup use `.detach()`,
  which is fire-and-forget. This is a correctness issue: the guest observes
  status=0 (reset complete) while DMA may still be in flight.

  **Root cause**: The `MmioIntercept::mmio_write()` trait method is sync
  (`fn mmio_write(&mut self, ...) -> IoResult`), so the transport write handler
  can't `.await`. However, the framework has `IoResult::Defer(DeferredToken)` —
  a mechanism to return from a sync handler and complete the IO asynchronously.

  **Key insight**: The transport holds `&mut self` during the write handler, so
  the guest cannot read `device_status` until the handler returns. If we use
  `IoResult::Defer`, the chipset framework will block the guest's write until
  `DeferredWrite::complete()` is called, at which point the guest can observe
  status=0. This satisfies VIRTIO spec §4.1.4.3.1.

  **Approach**: Decide between A or B (see notes.md for full analysis):

  **Approach A: Make `disable()` return a future**

  Change the `VirtioDevice` trait:
  ```rust
  fn disable(&mut self) -> impl Send + Future<Output = ()>;
  ```

  The transport's write handler returns `IoResult::Defer`, spawns the disable
  future, and calls `DeferredWrite::complete()` when the future resolves.

  Impact:
  - `VirtioDevice` trait — change signature
  - All 4 implementors (virtio_blk, virtio_net, virtio_pmem, virtiofs) — return futures
  - `LegacyWrapper::disable()` — return a future (stop workers + wait for descriptors)
  - PCI transport `write_bar_u32` — `IoResult::Defer` + spawn + complete
  - MMIO transport `write_u32` — same
  - PCI & MMIO `Drop` impls — still `.detach()` (Drop can never await; acceptable for VMM shutdown)

  **Approach B: Keep sync `disable()`, add completion signaling**

  `disable()` stays sync and kicks off shutdown. Add a completion mechanism
  (e.g., `disable()` returns a `OneshotReceiver<()>` or takes a completion
  callback). Transport stores a pending `DeferredWrite` and completes it when
  the receiver signals.

  Impact: Similar to A but avoids async in the trait itself. Adds channel plumbing.

  **Decision**: Leaning A or B — TBD. Both are viable. A is cleaner conceptually;
  B has slightly less trait change.

  Steps (will be refined once approach is chosen):

- [ ] **8.1 Choose approach (A or B) and prototype in one transport**

  Prototype the chosen approach in the PCI transport first. Key questions:
  - How does `IoResult::Defer` interact with the existing write handler flow?
  - Does `DeferredWrite::complete()` need to be called from a specific context?
  - Can the transport spawn a task that holds the `DeferredWrite`?

- [ ] **8.2 Update `VirtioDevice` trait**

  Apply the trait change. For approach A: `fn disable(&mut self) -> impl Send + Future<Output = ()>`.
  For approach B: `fn disable(&mut self) -> DisableHandle` or similar.

- [ ] **8.3 Update all `VirtioDevice` implementors**

  Update all 4 device impls + `LegacyWrapper` to match the new signature.
  Most implementations already have the async work ready — they just need
  to return the future instead of `.detach()`-ing it.

- [ ] **8.4 Update PCI transport**

  When device_status write of 0 triggers `disable()`:
  - Call `disable()` to get the future/handle
  - Create `DeferredWrite` via `defer_write()`
  - Spawn a task that awaits the disable future, then calls `deferred.complete()`
  - Return `IoResult::Defer(token)` from `mmio_write()`

  Note: currently `write_bar_u32` doesn't return `IoResult` — it's called from
  the `mmio_write` handler which always returns `IoResult::Ok`. This routing
  needs to be updated so the deferred token can propagate.

- [ ] **8.5 Update MMIO transport**

  Same pattern as PCI.

- [ ] **8.6 Handle `Drop` impls**

  `Drop` impls cannot await. Keep `.detach()` for Drop — this only fires during
  VMM shutdown when we don't need to wait for cleanup. Document this tradeoff.

- [ ] **8.7 Test**

  - Verify guest reset works correctly (Linux `vp_reset()` poll succeeds)
  - Verify no data loss on disable (IOs drain before guest sees status=0)
  - Verify stop/start cycle preserves in-flight IOs (from Phase 7)

## Notes

- See `ai/notes.md` for detailed codebase research and reference material
- The virtio PCI transport already exists; we just need to create the device and
  wrap it in `VirtioPciDeviceHandle`
- `RequestBuffers` bridge is the trickiest part — needs careful page alignment handling
- `VirtioDevice::enable()` receives `Resources` with queue params, features, interrupts;
  we spin up async worker tasks that poll the virtqueue for requests
- Guest memory is used both for reading descriptor data and for DMA (read/write disk data)
- Trust boundary: device must not panic on any guest input (malformed requests, OOB sectors, etc.)
