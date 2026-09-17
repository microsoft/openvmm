# OpenVMM fd-passing protocol

The **fd-passing protocol** lets a client hand pre-opened file descriptors to
the OpenVMM management server, each under a client-chosen name. A name can then
be referenced from the ordinary ttrpc/gRPC API — for example a `TapBackend` may
give an `fd_name` instead of a device `name`, so the server uses an already-open
tap fd instead of opening `/dev/net/tun` itself.

It runs over the **same `AF_UNIX` stream socket** as ttrpc and gRPC, and carries
descriptors via `SCM_RIGHTS`. It is UNIX-only.

Status: **revision 1** (handshake magic `0xFD 'F' 'D' 0x01`).

## Conventions

- Multi-byte integers are **little-endian**; `u8`/`u16`/`u32` are unsigned.
- **MUST** / **MAY** mark normative requirements. Blocks labelled *Rationale*
  are non-normative and explain intent only.
- Descriptors are transferred with `SCM_RIGHTS` control messages on
  `sendmsg`/`recvmsg` (see `unix(7)`, `cmsg(3)`).

## Client procedure

A client does the following, in order:

1. **Connect** to the socket.
2. **Send** the 8-byte handshake (see [Handshake](#handshake)), with
   `features = 0` and no ancillary data.
3. **Receive and validate** the server's 8-byte handshake. On magic mismatch or
   EOF, abort — the server does not speak this revision.
4. **For each descriptor:** send one `Register` request carrying the fd (see
   [Requests](#requests)), then read exactly one response (see
   [Responses](#responses)) before doing anything else.
5. **Optionally** `Deregister` names no longer needed.
6. **Close** the connection when done; any names still registered are dropped
   automatically (see [Lifetime and cleanup](#lifetime-and-cleanup)).

Requests and responses are strictly one-for-one: a client MUST read the response
to a request before sending the next request.

## Handshake

Both sides send a fixed 8-byte handshake; the client sends first, then the
server replies. Neither side sends anything else until both handshakes have been
exchanged.

```
Handshake (8 bytes):
  magic: [u8; 4]   // 0xFD, 0x46 'F', 0x44 'D', 0x01
  features: u32    // 0 in this revision
```

- Both sides MUST send exactly these 8 bytes, with `features = 0` and **no
  ancillary data**.
- Each side MUST verify the received 4-byte magic and MUST close the connection
  on any mismatch.
- A client MUST ignore (not validate) the received `features` value.
- If the server closes before sending its handshake (EOF), the client MUST treat
  it as not supporting this revision.

> *Rationale.* The leading `0xFD` differs from the ttrpc (`0x00`) and gRPC
> (`'P'`) first bytes, letting the server route a connection by its first byte.
> The 4-byte magic encodes both protocol and revision: a wire-incompatible
> future revision uses a *different* magic (advertised out of band via ttrpc),
> so there is no separate version field. `features` reserves room for optional,
> backward-compatible capabilities; sending 0 and ignoring the peer's value lets
> a newer peer advertise features without breaking an older one. Keeping the
> handshake free of ancillary data means descriptors are only ever attached to
> `Register`, with no ambiguity about which message carries an fd.

## Requests

After the handshake the connection is a synchronous request/response loop. Each
request is a fixed frame:

```
opcode: u8            // 1 = Register, 2 = Deregister
name_len: u8          // name length in bytes, 1..=255
name: [u8; name_len]  // UTF-8 identifier chosen by the client
```

### Register (opcode 1)

Registers one descriptor under `name`.

- The request MUST attach **exactly one** descriptor, in a single `SCM_RIGHTS`
  control message on the same `sendmsg` as the request bytes.
- Registration fails if `name` is already registered by **any** connection
  (names share one global namespace).
- On success the server owns the descriptor; the client MAY close its own copy
  once it has read the success response.

### Deregister (opcode 2)

Removes `name`. No descriptor is attached.

- A connection MAY only deregister names it registered itself; deregistering an
  unknown name, or a name owned by another connection, fails.
- On success the server drops its descriptor for that name.

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
registering with zero or more than one attached descriptor; an empty or
non-UTF-8 name; deregistering a name this connection did not register.

The server MUST NOT panic on any input. On a frame it cannot interpret — an
unknown opcode, or a truncated request — it closes any received descriptors and
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
  (cleanly or not), the server drops every descriptor it registered.
- The namespace is **global**: a name registered on one connection is resolvable
  from a *different* connection.
- Resolving a name **dups** the descriptor; the registry entry stays valid.
  Deregistering, or closing the registering connection, does not invalidate a
  descriptor already handed to a running VM.

> *Rationale.* A global namespace is required because the ttrpc/gRPC connection
> that creates a VM is separate from the fd-passing connection that registered
> the fd. Per-connection ownership guarantees cleanup even if the client
> crashes; dup-on-resolve lets a name be used more than once and decouples the
> VM's lifetime from the registration's.

## Referencing a registered fd

A registered name is used from the ordinary ttrpc/gRPC API. The first consumer
is the TAP backend:

```proto
message TapBackend {
  oneof source {
    string name    = 1;  // open /dev/net/tun by device name (existing behavior)
    string fd_name = 2;  // use the fd registered under this name
  }
}
```

With `fd_name`, the server resolves the name (dup) and uses it as the tap
backing fd; an unregistered name fails NIC creation.

## Wire format examples

Hex bytes; ancillary data is shown separately.

| Message | Bytes | Ancillary |
|---------|-------|-----------|
| Handshake (either side) | `FD 46 44 01 00 00 00 00` | none |
| `Register("t0", fd)` | `01 02 74 30` | one fd via `SCM_RIGHTS` |
| `Deregister("t0")` | `02 02 74 30` | none |
| Response, success | `00 00 00` | none |
| Response, failure `"boom"` | `01 04 00 62 6F 6F 6D` | none |

(`74 30` is the ASCII name `"t0"`; `04 00` is `msg_len = 4`, little-endian.)

## Implementing a client

Encoding and decoding are plain fixed byte buffers — `read_exact`/`write_all`,
no library needed. The only OS-specific step is attaching the fd on `Register`:
`std::os::unix::net::UnixStream` cannot pass a descriptor, so that one `sendmsg`
must build an `SCM_RIGHTS` control message. Options: call `libc::sendmsg`
directly; reuse OpenVMM's helper (the `SCM_RIGHTS` send/recv lives in the
`unix_socket` crate); or use a crate such as `sendfd`/`passfd`. The handshake,
`Deregister`, and all responses use no ancillary data.
