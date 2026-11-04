# OpenTMK Executor

`opentmk_executor` is a UEFI payload that receives generated programs over a
serial protocol and executes registered low-level operations.

## Intended use

The executor provides a small, deterministic environment for an external test
generator such as syzkaller. Instead of rebuilding the guest for each case, the
host boots the executor once, sends encoded programs, and receives an
acknowledgement or error packet for each case.

It is distinct from the configuration-driven `opentmk` test payload. The
executor has no embedded test selection.

## Build

Install the architecture's Rust UEFI target, then build the binary. For x64:

```bash
rustup target add x86_64-unknown-uefi
cargo build -p opentmk_executor --target x86_64-unknown-uefi
```

The output is a UEFI executable. Package it in a bootable image using the same
UEFI image tooling used by other guest test applications, or integrate it into
the external harness that owns the VM and serial channel.

## Serial channel

COM1 carries the executor protocol. After OpenTMK runtime initialization, the
guest performs a three-way handshake with fixed SYN, SYN-ACK, and ACK values.
Normal packets are framed with header and footer magic values and carry a
serialized `OpenTMKPacket`.

Do not use COM1 for human-readable logging while driving the protocol. The
runtime uses another serial path for logs so arbitrary text does not corrupt
packet framing.

## Packet sequence

A host session follows this order:

1. Complete the serial handshake.
2. Send a `Configuration` packet selecting a grammar deserializer and providing
   its function mapping.
3. Wait for an `Ack` packet.
4. Send one or more `FuzzTest` packets containing encoded programs.
5. Read an `Ack` with the program result code after each test.
6. Treat an `Error` packet as a terminal protocol or execution failure.

## Memory safety boundary

Decoded programs refer to generated memory through the `SafeMemoryMap`
interface. Read operations place their result into that map rather than
treating generated addresses as native pointers. Function implementations
validate arity and variable types before issuing privileged operations.

The environment still deliberately performs low-level I/O and hypercalls. Run
it only in a disposable test VM controlled by the fuzzing harness.

## Failure behavior

Bad handshake magic, packet framing, serialization, mappings, or packet order
produces an `OpenTMKErrorPacket` when possible and ends the executor loop.
Individual generated operations can return a nonzero acknowledgement code
without necessarily invalidating the session.

If the host appears to hang:

- Verify that the UEFI payload reached OpenTMK runtime initialization.
- Confirm both sides use the same packet and grammar definitions.
- Check that COM1 is a clean binary channel with no terminal translation.
- Ensure the host waits for configuration acknowledgement before sending a
  test case.

```admonish note title="See also"
[OpenTMK](opentmk.md) describes the configuration-driven UEFI test payload.
```
