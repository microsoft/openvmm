# How mesh works

This page covers mesh internals: the crate structure, the port and node
model, serialization engine, resource transfer, and cross-process
transport. You need this if you are working on mesh itself, debugging
subtle channel behavior, or reviewing changes to mesh infrastructure.

## Crate structure

Most code only needs the `mesh` facade crate. The lower crates matter
when working on mesh internals:

| Crate | Role |
|-------|------|
| `mesh` | Facade; re-exports channels, derive macros, and core types |
| `mesh_channel` / `mesh_channel_core` | Typed channel implementations and utilities |
| `mesh_node` | `Port`: the bidirectional binary channel primitive |
| `mesh_protobuf` | Encoding/decoding engine (protobuf superset); `no_std` |
| `mesh_derive` | `#[derive(MeshPayload)]` and `#[derive(Protobuf)]` proc macros |
| `mesh_worker` | `Worker` trait and `WorkerHost` |
| `mesh_process` | Multi-process mesh creation and child process spawning |
| `mesh_remote` | Platform IPC transports |
| `mesh_rpc` | gRPC / ttrpc server and client built on mesh channels |
| `mesh_build` | Protobuf service code generation (`build.rs` helper) |

## Ports and nodes

A `Sender<T>` / `Receiver<T>` pair starts as a simple in-memory queue—
no ports, no serialization. `send()` pushes a value; `recv()` pops one.
This is the fast, common-case path.

When a channel endpoint needs to cross a process boundary (because it's
being sent inside a message to a remote node), the channel transitions to
a port-backed `Remote` state. A `Port` is created, and from that point on
messages are serialized through the port rather than moved through the
in-memory queue. The `Port` is a bidirectional, untyped message channel
that the transport layer knows how to ship between processes.

Each port has a `PortId`—a cryptographically random 128-bit value. Ports
belong to a _node_ (identified by `NodeId`). Within a single process, all
ports share a "local node" and messages are passed directly (no
serialization). When a port's peer is on a _remote_ node (in another
process), messages are serialized and sent over the IPC transport.

This local/remote distinction is invisible to the channel layer above.
Channels hold a `Port`, and the port decides at send time whether to copy
the message in memory or serialize it to the wire. This is why code that
uses `Sender<T>` / `Receiver<T>` doesn't change when a component moves
to another process. Note that the transition to port-backed mode is
one-way: a channel that has been promoted to use a port stays in that
mode even if both ends later reside in the same process.

### Port mobility

Ports can be sent inside messages. When a port arrives at a new node,
its peer is notified of the new location via an internal `ChangePeer`
event. The peer updates its routing directly—future messages go straight
to the new node without passing through the old one.

This enables dynamic topologies. A parent process can create a channel,
send one end to child A, and child A can forward that end to child B.
Once the `ChangePeer` notification arrives, the parent sends messages
directly to child B. There is no permanent routing through A.

During the transition, messages are buffered. The port goes through a
`Sending` → `Proxying` state machine that ensures no messages are lost:
queued messages at the old location are forwarded to the new one, and
the peer doesn't start sending to the new location until it's confirmed
ready.

### Multi-node topology

Each process has a `LocalNode` that maintains a set of `RemoteNode`
connections—one per peer process. There is no central router. Each node
independently tracks which remote nodes it knows about.

`mesh_process` creates a **star topology**: the parent has a direct IPC
connection to each child. Children do not start with connections to each
other. But when a port is sent from one child to another (e.g., the
parent sends child A's port to child B), the nodes establish a new direct
connection so that messages flow between A and B without routing through
the parent. This means the set of inter-process connections grows
as ports migrate.

### Bridging

`Port::bridge()` connects two ports' peer links, creating a forwarding
chain. If port P1 is peered with port P2, and port P3 is peered with
port P4, then `P2.bridge(P3)` causes messages from P1 to flow to P4
(and vice versa). P2 and P3 act as proxies during the transition, then
send `ChangePeer` events so that P1 and P4 communicate directly.

This is used internally to splice channels together—for example, when a
worker is restarted and its channels need to be reconnected to new
endpoints.

## Encoding

`mesh_protobuf` is the serialization engine. It encodes messages in a
superset of
[Protocol Buffers](https://protobuf.dev/programming-guides/encoding/),
so standard protobuf tools can decode mesh messages (as long as they don't
contain resources).

The key extension over protobuf is a sideband _resource_ list attached to
each serialized message. Resources are things that cannot be serialized to
bytes but can be transferred between processes:

| Resource type | Platform |
|---------------|----------|
| `Port` (another channel endpoint) | All |
| `OwnedFd` (file descriptor) | Unix |
| `OwnedHandle` (kernel handle) | Windows |
| `OwnedSocket` (Winsock socket) | Windows |

The encoding is by-value: it consumes the source object and produces the
destination object. This is different from serde, which takes `&self`. The
by-value model is what allows ownership of OS handles, file descriptors,
and channel endpoints to be transferred rather than copied.

`#[derive(MeshPayload)]` generates encoders that use positional field
encoding. `#[derive(Protobuf)]` generates encoders that use tagged field
encoding (standard protobuf). Both support resources via
`#[mesh(resource = "Resource")]`, but messages containing resources
cannot be fully decoded by external protobuf tools.

## Cross-process transport

`mesh_remote` implements the platform-specific transports that carry
serialized messages and resources between processes:

- **Linux:** Unix domain sockets. Resources (file descriptors and ports)
  are transferred via `SCM_RIGHTS` ancillary messages.
- **Windows:** ALPC (Advanced Local Procedure Call). Resources (handles
  and ports) are transferred via ALPC handle duplication.

### Joining a mesh

There are two ways a process can join a mesh:

**Child process launch** (`mesh_process`). The parent spawns a child
process from the same binary and hands it an invitation via an
environment variable (the child is not a separate executable). This is
the model used by OpenVMM and OpenHCL for their worker processes — the
parent controls what the child can access by choosing which channel
endpoints and resources to include in the launch parameters. On Unix,
the invitation includes a pre-connected socket FD duplicated into the
child — the FD itself is the credential, non-guessable by other
processes. On Windows, the invitation includes an inherited object
directory handle and a 256-bit random `MeshSecret`, validated with
constant-time comparison.

**External process join** (`mesh_remote` listeners). A process binds
a `UnixMeshListener` or `AlpcMeshListener` to a well-known path and
waits for connections. This is used when the joining process is not a
child of the listening process. On Unix, security depends on
filesystem permissions — the socket path must be in a directory
accessible only to the intended user (e.g., `$XDG_RUNTIME_DIR` with
mode `0700`). On Windows, the `MeshSecret` provides cryptographic
authentication regardless of filesystem permissions.

### Process lifecycle

`mesh_process::Mesh::new()` creates the parent side of a mesh. Each call
to `launch_host()` spawns a child process, passing an invitation token
via an environment variable. The child calls `try_run_mesh_host()` early
in `main()` to accept the invitation and establish the IPC connection
back to the parent. Once connected, the child's `RemoteNode` transitions
from `Queuing` (buffering messages) to `Active` (sending over the live
connection).

If a child process crashes or the IPC connection is lost, the
`RemoteNode` transitions to `Failed`. All ports peered with that node
receive errors, which propagate up as `RecvError::Error` to any
`Receiver` waiting on them.

### Security

Port IDs and node IDs are cryptographically random 128-bit values
(generated via `getrandom`). A process can only send messages to a port
if it has been given a reference to that port—it cannot guess port
addresses. This limits the blast radius of a compromised child process:
it can only interact with ports it was explicitly given.

## Point-to-point mesh

`PointToPointMesh` is a lightweight, two-node mesh over any
bidirectional byte stream — a TCP connection, a Unix socket, a Windows
named pipe, or a vsock/hvsock connection. It does not support OS
resource transfer (no file descriptors or handles), since the
underlying stream may not support it (e.g., TCP across machines). If a
message containing OS resources is accidentally sent, the resources are
silently dropped and the affected port becomes stuck — avoid sending
OS resources over a point-to-point mesh.

This is a separate mechanism from the multi-process mesh described
above. It's used when two processes need mesh channels but aren't in a
parent/child relationship and may not even be on the same machine. The
petri test framework uses it to communicate between the host and a
test agent running inside a guest VM over hvsock.
