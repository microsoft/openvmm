# Using mesh

This page shows how to use mesh channels to exchange messages between
OpenVMM components.

## The pattern

In OpenVMM, components don't call each other directly. Instead, they
exchange messages through channels. One component sends a request into a
channel; another receives it, processes it, and (optionally) sends back a
response through an embedded reply channel. The two components don't need
to know where each other lives—same thread, different thread, different
process—mesh handles it.

This pattern shows up everywhere. The VM worker sends commands to device
emulators. The diagnostics server sends RPCs to the VM worker. Device
workers in child processes receive I/O requests from the main process.
All of these use the same mesh channel types. (When we say "child
process" here, we mean a separate OS process forked from the same
binary — not a separate executable. The parent chooses which channel
endpoints and resources to pass to each child.)

To see how this works, let's build a simple disk controller that accepts
read and write commands. We'll start with the basics and build up to the
patterns you'll see in production code.

## Defining messages

The first step is defining what messages your component accepts. Any type
can be sent over a mesh channel as long as both ends are in the same
process—there's no trait requirement for in-process use. But if the
channel might cross a process boundary (which is common, since the whole
point of mesh is making that transparent), the type needs to implement
`MeshPayload`. The easiest way is to derive it:

```rust,ignore
#[derive(MeshPayload)]
struct DiskRead {
    offset: u64,
    len: u32,
}
```

All fields must also implement `MeshPayload`. Most standard types already
do: `String`, integers, `bool`, `Vec<T>`, `Option<T>`, `HashMap<K, V>`,
etc.

But a read request isn't useful without a way to get data back. mesh
solves this by letting you put _channel endpoints_ inside messages. The
caller creates a oneshot channel, puts the send half in the request, and
awaits the receive half:

```rust,ignore
#[derive(MeshPayload)]
struct DiskRead {
    offset: u64,
    len: u32,
    result: mesh::OneshotSender<Vec<u8>>,  // reply channel
}
```

This is so common that mesh provides [`Rpc<I, R>`](https://openvmm.dev/rustdoc/linux/mesh/rpc/struct.Rpc.html),
which bundles the request and reply channel together (it lives in
`mesh::rpc::Rpc`). And the conventional pattern is to define a _request
enum_ so a single channel can carry multiple command types:

```rust,ignore
use mesh::rpc::Rpc;

#[derive(MeshPayload)]
enum DiskRequest {
    Read(Rpc<DiskReadParams, Vec<u8>>),
    Write(Rpc<DiskWriteParams, ()>),
    Flush(Rpc<(), ()>),
    GetInfo(Rpc<(), DiskInfo>),
}

#[derive(MeshPayload)]
struct DiskReadParams {
    offset: u64,
    len: u32,
}

#[derive(MeshPayload)]
struct DiskWriteParams {
    offset: u64,
    data: Vec<u8>,
}

#[derive(MeshPayload)]
struct DiskInfo {
    size_bytes: u64,
    sector_size: u32,
}
```

For operations that can fail, use `FailableRpc<I, R>`, which wraps the
response in `Result<R, RemoteError>`:

```rust,ignore
#[derive(MeshPayload)]
enum DiskRequest {
    Read(FailableRpc<DiskReadParams, Vec<u8>>),
    Write(FailableRpc<DiskWriteParams, ()>),
    Flush(FailableRpc<(), ()>),
    GetInfo(Rpc<(), DiskInfo>),  // this one can't fail
}
```

## Handling requests

Now let's write the disk controller. It receives `DiskRequest` messages
from a `Receiver` and handles each one:

```rust,ignore
async fn disk_service(
    mut rx: mesh::Receiver<DiskRequest>,
    disk: &mut DiskBackend,
) {
    loop {
        match rx.recv().await {
            Ok(req) => match req {
                DiskRequest::Read(rpc) => {
                    rpc.handle(async |p| {
                        disk.read(p.offset, p.len).await
                    }).await
                }
                DiskRequest::Write(rpc) => {
                    rpc.handle(async |p| {
                        disk.write(p.offset, &p.data).await
                    }).await
                }
                DiskRequest::Flush(rpc) => {
                    rpc.handle(async |()| disk.flush().await).await
                }
                DiskRequest::GetInfo(rpc) => {
                    rpc.handle_sync(|()| disk.info())
                }
            },
            Err(_) => break, // all senders dropped; shut down
        }
    }
}
```

A few things to note:

- **`.handle(async |input| ...).await`** is for async handlers. The
  closure runs, and the return value is automatically sent back to the
  caller.
- **`.handle_sync(|input| ...)`** is for synchronous handlers, when the
  result is available immediately (no `.await` needed).
- **`recv()` returns `Err` when the channel closes**—either all senders
  were dropped (`RecvError::Closed`) or the remote process went away
  (`RecvError::Error`). Either way, the service shuts down.

## Sending requests

On the other side, the caller creates a channel and uses `.call()` to
send requests and await responses:

```rust,ignore
let (tx, rx) = mesh::channel::<DiskRequest>();

// Hand `rx` to the disk service (possibly in another process).
spawn.spawn("disk", disk_service(rx, &mut backend));

// Send requests and get responses.
let info = tx.call(DiskRequest::GetInfo, ()).await?;
println!("disk size: {} bytes", info.size_bytes);

let data = tx
    .call_failable(DiskRequest::Read, DiskReadParams {
        offset: 0,
        len: 512,
    })
    .await?;
```

`.call()` takes an enum variant constructor and the input value, creates
the `Rpc` internally (including the reply channel), sends it, and returns
a future for the response. `.call_failable()` does the same for
`FailableRpc`, combining the channel error and application error into a
single `RpcError`.

`Sender<T>` can be cloned, making channels multi-producer: multiple
components can hold clones of the same sender and send messages to a
single receiver. The channel stays open until _all_ sender clones are
dropped.

### Fire-and-forget

`send()` is also available when you don't need a response:

```rust,ignore
let (tx, mut rx) = mesh::channel::<String>();
tx.send("hello".into());
```

`send()` never fails from the caller's perspective—if the receiver is
gone, the message is silently dropped. This is useful for notifications
and events but not for commands where you need confirmation.

### Backpressure

mesh channels are **unbounded**: `send()` never blocks, and messages
queue in memory until the receiver consumes them. There is no built-in
bounded channel currently; this may be added in the future. Unbounded
channels are fine when the receiver processes messages at least as fast
as the sender produces them (which is the common case for RPC-style
usage, where the caller awaits a response before sending the next
request).

If you have a producer that can outrun its consumer—e.g., streaming
data—use `mesh::pipe()` instead, which provides backpressure via
`AsyncWrite` (the writer blocks when the internal buffer is full). For
structured messages, you can build your own backpressure by having the
consumer send a reply or acknowledgment that the producer awaits before
sending more.

## Resources in messages

What makes mesh different from a plain channel or serialization framework
is that messages can carry _resources_—things that can't be serialized to
bytes but can be transferred between processes. If the two ends are in the
same process, resources are just moved. If they're in different processes,
mesh transfers them over the OS IPC mechanism automatically.

```rust,ignore
#[derive(MeshPayload)]
struct DiskWorkerParams {
    config: DiskConfig,
    commands: mesh::Receiver<DiskRequest>,  // channel endpoint
    backing_file: OwnedFd,                 // file descriptor (Unix)
}
```

The supported resource types:

| Resource type | Platform |
|---------------|----------|
| `Sender<T>`, `Receiver<T>` (channel endpoints) | All |
| `OwnedFd` (file descriptor) | Unix |
| `OwnedHandle` (kernel handle) | Windows |
| `OwnedSocket` (Winsock socket) | Windows |

This is how mesh enables the multi-process model: create a channel pair,
put one end in a message along with any file descriptors the child needs,
and send it to a worker in another process. The worker receives a live
channel endpoint and open file handle, ready to use.

## Other channel types

The Rpc pattern covers most use cases, but mesh provides a few other
channel types for specific situations.

### Oneshot

For transferring a single value, `mesh::oneshot()` is lighter than a
full channel — it has no queue, no cloning (single producer, single
consumer), and less internal bookkeeping. It also makes the intent
explicit: the type system ensures exactly one value is sent, so the
compiler catches accidental reuse:

```rust,ignore
let (tx, rx) = mesh::oneshot::<DiskInfo>();
tx.send(info);
let info = rx.await?;
```

### Cell

`Cell<T>` / `CellUpdater<T>` is a publish-subscribe primitive for pushing
configuration updates. The updater creates cells and broadcasts changes:

```rust,ignore
let mut updater = CellUpdater::new(initial_config);
let cell = updater.cell(); // send to a subscriber
updater.set(new_config);   // all cells see the update
```

### Pipe

`mesh::pipe()` returns a `(ReadPipe, WritePipe)` pair implementing
`AsyncRead` / `AsyncWrite` over mesh, with backpressure. Useful for
streaming byte data like console output.

### Cancel

`CancelContext` provides cooperative cancellation with optional deadlines,
transferable across process boundaries:

```rust,ignore
let mut ctx = CancelContext::new();
ctx.with_deadline(Duration::from_secs(5));
ctx.cancelled().await; // resolves on cancel or deadline
```

## Workers

So far, we've been connecting components with channels directly. Workers
go a step further: they're self-contained components with a defined
lifecycle (start, stop, hot-restart) that can optionally run in a
separate process.

Use a worker when your component needs:
- To be **stoppable and restartable** (e.g., for OpenHCL servicing).
- To run in a **separate process** for isolation or security.
- To be **inspectable** at runtime via the diagnostics system.

If you just need to exchange messages, plain channels or Rpc are simpler.

Here's our disk controller as a worker:

```rust,ignore
impl Worker for DiskWorker {
    type Parameters = DiskWorkerParams;
    type State = DiskWorkerState;
    const ID: WorkerId<Self::Parameters> = WorkerId::new("disk");

    fn new(params: Self::Parameters) -> anyhow::Result<Self> {
        let backend = DiskBackend::open(params.backing_file)?;
        Ok(Self { backend, commands: params.commands })
    }

    fn restart(state: Self::State) -> anyhow::Result<Self> {
        // Reconstruct from saved state (for hot restart during
        // servicing). The state was produced by a prior instance's
        // Restart handler.
        Ok(Self { /* ... */ })
    }

    fn run(self, recv: mesh::Receiver<WorkerRpc<Self::State>>)
        -> anyhow::Result<()>
    {
        // Run until Stop or channel close.
        // Handle WorkerRpc::Stop, Restart, and Inspect.
        Ok(())
    }
}
```

- **`Parameters`** is the data needed to start the worker. It must be
  `MeshPayload` so it can be sent to a child process.
- **`State`** is the data needed to restart the worker without losing work
  (hot restart). Used during OpenHCL servicing to update the paravisor
  without rebooting the VM.
- **`run()`** is the main loop. It receives `WorkerRpc` messages (`Stop`,
  `Restart`, `Inspect`) alongside whatever other channels the worker
  holds.

Workers are registered at compile time and launched by a `WorkerHost`:

```rust,ignore
register_workers! { DiskWorker, TpmWorker }

let (host, runner) = mesh_worker::worker_host();
spawn.spawn("worker-host", runner.run(RegisteredWorkers));
let handle = host.launch_worker(DISK_WORKER, params).await?;
```

When the worker host runs in a child process (via `mesh_process`), the
same `launch_worker` call spawns the worker there instead.

## Quick reference

| I want to... | Use |
|--------------|-----|
| Send a message, no reply needed | `Sender<T>` / `Receiver<T>` |
| Request/response | `Rpc<I, R>` or `FailableRpc<I, R>` |
| Transfer a single value | `OneshotSender<T>` / `OneshotReceiver<T>` |
| Push config updates to subscribers | `CellUpdater<T>` / `Cell<T>` |
| Stream bytes with backpressure | `pipe()` → `ReadPipe` / `WritePipe` |
| Cancel an async operation | `CancelContext` |
| Component with lifecycle + isolation | `Worker` trait + `WorkerHost` |
