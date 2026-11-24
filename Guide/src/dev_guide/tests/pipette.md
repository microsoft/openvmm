# Pipette Guest Agent

`pipette` is the in-guest agent through which Petri tests execute commands and
perform operating-system operations.

## Role in a VMM test

Petri divides guest control between a host client and the Pipette agent:

```text
VMM test process
  `- Pipette client
       `- VSocket or TCP connection
            `- Pipette agent in the guest
                 |- execute process
                 |- transfer files
                 |- collect output
                 `- shut down the guest
```

The test obtains a `PipetteClient` after the VM boots. Commands created through
that client are serialized to the guest, executed there, and represented by a
remote child process whose status and standard streams are returned to the
host.

Pipette is test infrastructure, not an OpenVMM management protocol. Its API and
wire format are shared by Petri components and can change with the repository.

## Supported guests

Pipette runs in Linux and Windows guests on the architectures built by the VMM
test pipeline. The binary is compiled for the guest, not necessarily for the
developer's host.

The default transport is VSocket. TCP is used when the guest configuration has
no usable VSocket path, including the current Windows `no-vmbus` image.

Windows builds also accept `--service`. The generated test-image registry hive
uses this mode so Pipette starts automatically as `LocalSystem`.

## Linux PID 1 mode

Some Linux direct-boot configurations use Pipette as `rdinit`. When its process
ID is 1, Pipette performs the minimal init responsibilities needed by the test
guest before starting the request agent. These include setting up essential
mounts and reaping child processes.

This mode deliberately does not try to become a general init system. The guest
image should contain only the services required by its test workload.

## Connection lifecycle

At startup, Pipette initializes its selected transport and establishes the
protocol connection. It then serves requests until the host disconnects. A
disconnect is not fatal: the process creates a new agent connection and waits
for the host again.

The exact request set is defined by the `pipette_protocol` crate. Tests should
normally use `pipette_client` rather than manually constructing protocol messages.

## Building Pipette

The supported path is to let `cargo xflowey vmm-tests-run` discover and build
the guest artifact required by the selected tests:

```bash
cargo xflowey vmm-tests-run --filter "test(my_test)" --build-only
```

For a focused build, choose a target matching the guest image. For example:

```bash
cargo build -p pipette --target x86_64-unknown-linux-musl
```

```bash
cargo build -p pipette --target x86_64-pc-windows-msvc
```

```admonish warning
A host-native `cargo build -p pipette` is only useful when the host target also
matches the intended guest. Petri resolves Pipette by guest OS and
architecture, so placing a binary under the wrong target directory does not
satisfy the artifact requirement.
```

## Image integration

Linux test images either include Pipette in an initrd or expose it through a
shared test-content disk. Windows images are prepared with a registry hive that
defines the Pipette service and points it at the injected executable.

The `prep_steps` tool prepares reusable Windows images. The `make_imc_hive`
tool regenerates the source hive when the service definition changes.

## Troubleshooting

If a test times out waiting for Pipette:

1. Check the guest serial log for Pipette startup or panic messages.
2. Confirm that the binary matches the guest architecture and operating system.
3. Confirm that the configured transport matches the VM's available VSocket or
   TCP path.
4. For Linux direct boot, verify that the kernel command line starts the
   intended init and that essential mounts succeeded.
5. For Windows, verify that the service image path matches the drive containing
   `pipette.exe`.

Incubator additionally waits for a `PIPETTE READY` serial marker before opening
its forwarded TCP connection. A missing marker generally means the outer guest
failed before Pipette initialized.
