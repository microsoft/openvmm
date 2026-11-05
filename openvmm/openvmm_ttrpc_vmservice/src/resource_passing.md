# OpenVMM resource-passing protocol

The **resource-passing protocol** lets a client hand OS resources to the OpenVMM
management server, each under a client-chosen name. A name can then be
referenced from the ordinary ttrpc/gRPC API — for example a `TapBackend` may give
an `fd_name` instead of a device `name`, so the server uses an already-open tap
fd instead of opening `/dev/net/tun` itself.

It runs over the **same `AF_UNIX` stream socket** as ttrpc and gRPC. Framing and
semantics are common to all hosts; only how a resource travels differs, by
**transport**:

| | fd-passing | handle-passing |
|-|------------|----------------|
| Host | UNIX | Windows |
| Resource | file descriptor | handle |
| Handshake magic | `FD 46 44 01` (`0xFD 'F' 'D' 0x01`) | `FD 48 50 01` (`0xFD 'H' 'P' 0x01`) |
| Resource in `Register` | one fd via `SCM_RIGHTS` | `handle: u64` after the name |
| Ancillary data | on `Register` only | never |

A server speaks only its host's transport. Both use only documented OS APIs.

Status: **revision 1** of each transport.

## Conventions

- Multi-byte integers are **little-endian**; `u8`/`u16`/`u32`/`u64` are
  unsigned.
- **MUST** / **MAY** mark normative requirements. Blocks labelled *Rationale*
  are non-normative and explain intent only.
- A *resource* is a file descriptor (fd-passing) or a handle (handle-passing).

## Client procedure

A client does the following, in order:

1. **Prepare** (handle-passing only): open the OpenVMM process with
   `PROCESS_DUP_HANDLE` access. A client that launched OpenVMM can use the
   process handle it already holds.
2. **Connect** to the socket.
3. **Send** the 8-byte handshake for its transport (see [Handshake](#handshake)),
   with `features = 0`.
4. **Receive and validate** the server's 8-byte handshake. On magic mismatch or
   EOF, abort — the server does not speak this transport or revision.
5. **For each resource:** send one `Register` request carrying it (see
   [Register](#register-opcode-1) and the transport's section), then read
   exactly one response (see [Responses](#responses)) before doing anything
   else.
6. **Optionally** `Deregister` names no longer needed.
7. **Close** the connection when done; any names still registered are dropped
   automatically (see [Lifetime and cleanup](#lifetime-and-cleanup)).

Requests and responses are strictly one-for-one: a client MUST read the response
to a request before sending the next request.

## Handshake

Both sides send a fixed 8-byte handshake; the client sends first, then the
server replies. Neither side sends anything else until both handshakes have been
exchanged.

```
Handshake (8 bytes):
  magic: [u8; 4]   // the transport's magic; see the table above
  features: u32    // 0 in this revision
```

- Both sides MUST send exactly these 8 bytes, with `features = 0` and **no
  ancillary data**.
- Each side MUST verify the received 4-byte magic and MUST close the connection
  on any mismatch. In particular, a server closes a connection that sends
  another host's magic.
- A client MUST ignore (not validate) the received `features` value.
- If the server closes before sending its handshake (EOF), the client MUST treat
  it as not supporting this transport or revision.

> *Rationale.* The leading `0xFD` differs from the ttrpc (`0x00`) and gRPC
> (`'P'`) first bytes, letting the server route a connection by its first byte.
> The 4-byte magic encodes transport and revision: a wire-incompatible future
> revision uses a *different* magic (advertised out of band via ttrpc), so there
> is no separate version field. `features` reserves room for optional,
> backward-compatible capabilities; sending 0 and ignoring the peer's value lets
> a newer peer advertise features without breaking an older one. Keeping the
> handshake free of ancillary data means resources are only ever attached to
> `Register`, with no ambiguity about which message carries one.

## Requests

After the handshake the connection is a synchronous request/response loop. Each
request begins with:

```
opcode: u8            // 1 = Register, 2 = Deregister
name_len: u8          // name length in bytes, 1..=255
name: [u8; name_len]  // UTF-8 identifier chosen by the client
```

### Register (opcode 1)

Registers one resource under `name`. The resource travels as defined by the
transport: see [fd-passing](#fd-passing-unix) and
[handle-passing](#handle-passing-windows).

- Registration fails if `name` is already registered by **any** connection
  (names share one global namespace).

### Deregister (opcode 2)

Removes `name`. No resource is attached and no bytes follow the name.

- A connection MAY only deregister names it registered itself; deregistering an
  unknown name, or a name owned by another connection, fails.
- On success the server drops its resource for that name.

## Responses

The server sends exactly one response per request, in order:

```
status: u8          // 0 = ok; non-zero = failure
msg_len: u16        // length of msg (0 on success)
msg: [u8; msg_len]  // UTF-8 diagnostic text (empty on success)
```

- `status = 0` is success; any non-zero value is failure.
- This revision emits only `status = 1` (**generic failure**). A client MUST
  treat every non-zero value as a single failure.
- `msg` is human-readable diagnostic text only; a client MUST NOT parse it.
- The connection stays usable after a failure response.

Failures include, at minimum: registering an already-registered name;
registering without a valid resource; an empty or non-UTF-8 name; deregistering
a name this connection did not register.

The server MUST NOT panic on any input. On a frame it cannot interpret — an
unknown opcode, or a truncated request — it drops any received resources and
closes the connection.

> *Rationale.* Reserving the non-zero code space lets a future revision assign
> specific codes while an older client keeps working by collapsing them to
> "failure". An uninterpretable frame cannot be skipped (there is no length
> prefix), so the byte stream cannot resynchronize and the connection must
> close. That is acceptable because the opcode set is fixed for a given
> handshake magic, and additive capabilities are negotiated via `features`, so a
> conforming client never sends one.

## Lifetime and cleanup

- Names are **owned by the registering connection**: when that connection closes
  (cleanly or not), the server drops every resource it registered.
- The namespace is **global**: a name registered on one connection is resolvable
  from a *different* connection.
- Resolving a name **duplicates** the resource; the registry entry stays valid.
  Deregistering, or closing the registering connection, does not invalidate a
  resource already handed to a running VM.

> *Rationale.* A global namespace is required because the ttrpc/gRPC connection
> that creates a VM is separate from the connection that registered the
> resource. Per-connection ownership guarantees cleanup even if the client
> crashes; duplicate-on-resolve lets a name be used more than once and decouples
> the VM's lifetime from the registration's.

## fd-passing (UNIX)

Descriptors are transferred with `SCM_RIGHTS` control messages on
`sendmsg`/`recvmsg` (see `unix(7)`, `cmsg(3)`).

- A `Register` request MUST attach **exactly one** descriptor, in a single
  `SCM_RIGHTS` control message on the same `sendmsg` as the request bytes. No
  bytes follow the name.
- No other message carries ancillary data.
- On success the server owns the descriptor; the client MAY close its own copy
  once it has read the success response.

### Implementing a client

Encoding and decoding are plain fixed byte buffers — `read_exact`/`write_all`,
no library needed. The only OS-specific step is attaching the fd on `Register`:
`std::os::unix::net::UnixStream` cannot pass a descriptor, so that one `sendmsg`
must build an `SCM_RIGHTS` control message. Options: call `libc::sendmsg`
directly; reuse OpenVMM's helper (the `SCM_RIGHTS` send/recv lives in the
`unix_socket` crate); or use a crate such as `sendfd`/`passfd`.

## handle-passing (Windows)

Windows `AF_UNIX` sockets cannot carry handles, so the client first duplicates
each handle into the OpenVMM process with `DuplicateHandle` and sends the
resulting handle **value** in the request. No message carries ancillary data.

A `Register` request is followed by the handle value:

```
opcode: u8            // 1
name_len: u8
name: [u8; name_len]
handle: u64           // a handle value valid in the OpenVMM process
```

To register a handle, the client:

1. Duplicates it into OpenVMM:
   `DuplicateHandle(GetCurrentProcess(), handle, openvmm, &remote, 0, FALSE, DUPLICATE_SAME_ACCESS)`.
2. Sends `Register` carrying `remote`, then reads the response.
3. Closes `remote` in OpenVMM, **whether or not registration succeeded**:
   `DuplicateHandle(openvmm, remote, NULL, NULL, 0, FALSE, DUPLICATE_CLOSE_SOURCE)`.

The client MUST NOT close `remote` until it has read the response.

- `handle` MUST be a handle the client duplicated into the OpenVMM process. The
  server does **not** take ownership of it: it makes its own duplicate (with the
  same access) and registers that.
- Registration fails if `handle` is zero, a pseudo-handle (any value that is
  negative as a signed 64-bit integer, such as `GetCurrentProcess()`), or not a
  valid handle in the OpenVMM process.

> *Rationale.* This is the only place OpenVMM interprets a client-supplied
> handle value; the rest of the API refers to handles by name. The server
> duplicates rather than adopts the value because it cannot tell which handles
> in its own process the value might alias: adopting, and later closing, an
> arbitrary value could close a handle owned by unrelated code. Duplicating
> cannot invalidate any other owner, and a client holding `PROCESS_DUP_HANDLE`
> on OpenVMM is already fully trusted over it. The client remains responsible
> for its `remote` copy, which it closes remotely with `DUPLICATE_CLOSE_SOURCE`.
> If the client exits before doing so, that one copy leaks until OpenVMM exits.

## Referencing a registered resource

A registered name is used from the ordinary ttrpc/gRPC API, which resolves it
(duplicating the resource) when the RPC is processed; an unregistered name fails
the RPC. Once the RPC succeeds, the name may be deregistered.

| Field | Host | Resource |
|-------|------|----------|
| `TapBackend.fd_name` | Linux | tap device fd, used instead of opening `/dev/net/tun` |
| `VirtioFs.WindowsOptions.user_token_name` | Windows | access token that virtio-fs impersonates for host file access |

## Wire format examples

Hex bytes; ancillary data is shown separately.

| Message | Bytes | Ancillary |
|---------|-------|-----------|
| Handshake, fd-passing (either side) | `FD 46 44 01 00 00 00 00` | none |
| Handshake, handle-passing (either side) | `FD 48 50 01 00 00 00 00` | none |
| `Register("t0", fd)`, fd-passing | `01 02 74 30` | one fd via `SCM_RIGHTS` |
| `Register("t0", 0x1A4)`, handle-passing | `01 02 74 30 A4 01 00 00 00 00 00 00` | none |
| `Deregister("t0")` | `02 02 74 30` | none |
| Response, success | `00 00 00` | none |
| Response, failure `"boom"` | `01 04 00 62 6F 6F 6D` | none |

(`74 30` is the ASCII name `"t0"`; `A4 01 00 00 00 00 00 00` is the handle value
`0x1A4`; `04 00` is `msg_len = 4`.)
