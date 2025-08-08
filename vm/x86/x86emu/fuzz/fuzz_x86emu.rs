// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg_attr(all(target_os = "linux", target_env = "gnu"), no_main)]
#![expect(missing_docs)]

use arbitrary::Arbitrary;
use cpu::FuzzerCpu;
use futures::FutureExt;
use x86defs::cpuid::Vendor;
use x86emu::Emulator;
use x86emu::Error;
use xtask_fuzz::fuzz_target;

mod cpu;

#[derive(Debug, Arbitrary)]
struct StaticParams {
    cpu: FuzzerCpu,
    vendor: Vendor,
    code: [u8; 16],
}

fn do_fuzz(static_params: StaticParams) -> arbitrary::Result<()> {
    let StaticParams { cpu, vendor, code } = static_params;

    let mut emu = Emulator::new(cpu, vendor, &code);
    emu.run().now_or_never().unwrap().or_else(|e| {
        match *e {
            // Acceptable results
            Error::InstructionException(_, _, _) => Ok(()),

            // Not useful results - didn't make it into meaningful code
            Error::DecodeFailure
            | Error::UnsupportedInstruction(_)
            | Error::NonMemoryOrPortInstruction(_) => Err(arbitrary::Error::IncorrectFormat),

            // Should be impossible as we provide the maximum length up front
            Error::NotEnoughBytes => unreachable!(),

            // Should be impossible given our simple cpu implementation
            Error::MemoryAccess(_, _, _) | Error::IoPort(_, _, _) => {
                unreachable!()
            }
        }
    })
}

fuzz_target!(|input: StaticParams| -> libfuzzer_sys::Corpus {
    xtask_fuzz::init_tracing_if_repro();
    if do_fuzz(input).is_err() {
        libfuzzer_sys::Corpus::Reject
    } else {
        libfuzzer_sys::Corpus::Keep
    }
});
