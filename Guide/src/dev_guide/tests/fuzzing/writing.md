# Writing Fuzzers

## Writing a new fuzzer in OpenVMM

The easiest way to get up and running is to look at the existing in-tree fuzzers
(which you can list using `cargo xtask fuzz list`), along with reading through
the [cargo-fuzz book](https://rust-fuzz.github.io/book/cargo-fuzz.html) (the
book is fairly brief and shouldn't take more than 20 minutes to read through)

Some examples of in-tree fuzzers:

* Simple device example: [chipset/fuzz/battery.rs][fuzz_battery_url]
* More complex device example: [ide/fuzz/fuzz_ide.rs][fuzz_ide_url]
* Abstraction over unsafe example: [ide/fuzz/fuzz_scsi_buffer.rs][fuzz_scsi_buffer_url]

[fuzz_battery_url]: https://github.com/microsoft/openvmm/blob/main/vm/devices/chipset/fuzz/fuzz_battery.rs
[fuzz_ide_url]: https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/ide/fuzz/fuzz_ide.rs
[fuzz_scsi_buffer_url]: https://github.com/microsoft/openvmm/blob/main/vm/devices/storage/scsi_buffers/fuzz/fuzz_scsi_buffers.rs

Once you're ready to take a stab at writing your own fuzzer, spinning up a new
fuzzer is as easy as running:

```bash
cargo xtask fuzz init openvmm_crate_to_fuzz TEMPLATE
```

Use `--help` for more details on the available TEMPLATE types.

```admonish caution
We don't suggest using `cargo fuzz init` (i.e: without `xtask`), as it
emits a template that isn't compatible with the OpenVMM repo style, and also
doesn't properly update the root Cargo.toml's `workspace.members` array.
```

## Fuzzing an abstraction over unsafe code

Unsafe code is a prioritized target for fuzzing given its self-evident risks.

However, it can be hard to reason about how best to exercise unsafe code in OpenVMM
via fuzzing as often (and correctly) the unsafe code is not directly interacting
with guest-controlled data.

The approach taken with OpenVMM then is to target abstractions over unsafe code.
Such as interfaces and data structures like `BounceBuffers`, `guest_memory`, or
`ucs2`. A fuzzer for one of these will attempt to be a regular consumer of the
abstraction, calling those APIs declared safe and using any data structure in a
rust-safe way. This attempts to check that the safety guarantees the abstraction
is making are being upheld via the API.

For example, let's say we want to fuzz `BounceBuffers`, here's what we may
want to fuzz:

![BounceBuffers Example](./_images//fuzz_abstraction_example.png "Overview of the safe API exposed by BounceBuffers")

Fuzz logic might then allocate a `BounceBuffer` using `new` and call methods on
it such as `as_mut_bytes` and `io_vecs`. Then it could access the return result
of both those calls:

```rust,ignore
use scsi_buffers::BounceBuffer;

#[derive(Arbitrary)]
enum BouneBufferAccess {
    AsMutBytes,
    IoVecs
}

#[derive(Arbitrary)]
struct FuzzCase {
    #[arbitrary(with = |u: &mut Unstructured| u.int_in_range(0..=0x40000))]
    size: usize,
    accesses: Vec<BounceBufferAccess>
}

fn access_mut_bytes(buf: &mut [u8]) {
    buf.fill(b'A');
    // access buf in other ways to test validity of underlying memory and slice
}

fn access_io_vecs(io_vecs: &[IoBuffer]) {
    // access each IoBuffer to test validity of ptr and len
}

fuzz_target!(|fuzz_case: FuzzCase| { do_fuzz(fuzz_case) });

fn do_fuzz(fuzz_case: FuzzCase) {
    let mut bb = BounceBuffer::new(fuzz_case.size);

    for access in fuzz_case.accesses {
        match access {
            AsMutBytes => {
                let buf = bb.as_mut_bytes();
                access_mut_bytes(buf);
            },
            IoVecs => {
                let io_vecs = bb.io_vecs();
                access_io_vecs(io_vecs)
            }
        }
    }
}
```

The fuzzer should work to ensure the safe members of the API cannot be misused
in any way that may result in memory corruption or unsoundness.

## Fuzzing a chipset device

Writing a fuzzer for a chipset device (e.g: battery, ide, serial, pic,
etc...) involves targeting the API that is roughly exposed to guests: the
device's port IO, PCI config, and MMIO interfaces.

While it's entirely possible to hand-roll a fuzzer that is tailored to the
specific register configuration of a particular device, the in-repo
`chipset_device_fuzz` crate exports a `FuzzChipset` type that offers a
"plug-and-play" way to hook a chipset device up to a fuzzer:

```rust,ignore
#[derive(Arbitrary)]
struct StaticDeviceConfig {
    #[arbitrary(with = |u: &mut Unstructured| u.int_in_range(0..=16))]
    num_queues: usize,
}

fn do_fuzz(u: &mut Unstructured<'_>) -> arbitrary::Result<()> {
    // Step 1: generate a device's fixed-at-construction-time configuration
    let static_device_config: StaticDeviceConfig = u.arbitrary()?;

    // Step 2: init the device, and wire-it-up to the fuzz chipset
    let mut chipset = chipset_device_fuzz::FuzzChipset::default();
    let my_device = chipset.device_builder("my_dev").add(|services| {
        my_dev::MyDevice::new(
            static_device_config.num_queues,
            &mut services.register_mmio(), // e.g: pci devices have BARs to remap their MMIO intercepts
        )
    }).unwrap();

    // Step 3: use the remaining fuzzing input to slam the device with chipset events
    while !u.is_empty() {
        let action = chipset.get_arbitrary_action(u)?;
        xtask_fuzz::fuzz_eprintln!("{:x?}", action); // only prints when running a repro
        chipset.exec_action(action).unwrap();

        // Step 3.5: (optionally) intersperse "external stimuli" between chipset actions
        if u.ratio(1, 10)? {
            let event: u32 = u.arbitrary()?;
            my_device.report_external_event(event);
        }
    }

    Ok(())
}

fuzz_target!(|input: &[u8]| -> libfuzzer_sys::Corpus {
    if do_fuzz(&mut Unstructured::new(input)).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
```

## Fuzzing a vmbus device

TBD (no such fuzzers exist in-tree today)

## Fuzzing `async` code

Depending on the nature of the 'async' code in question, there are two main
recommended approaches to fuzzing it:

1. **now_or_never**: The recommended approach for individual asynchronous calls
    is to use the `now_or_never` method from the `futures` crate. This method
    will poll the future to completion, but will not block if the future is
    not ready to complete. This allows you to fuzz the future without needing
    to run it to completion, which can be useful for testing the behavior of
    the future in various states.

2. **DefaultPool::run_with**: The recommended approach for more intricate
    asynchronous requirements is to use the `DefaultPool::run_with` method from
    our `pal_async` crate. This method takes a custom async function and runs it
    to completion. This allows you to write custom code using regular async/await
    syntax, combinators, `join`s, `select`s, or whatever you wish.
