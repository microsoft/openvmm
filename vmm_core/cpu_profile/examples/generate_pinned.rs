// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Generates `src/pinned_data.rs`, the pinned profiles as static data, from
//! their pinned JSON files.
//!
//! ```text
//! cargo run -p cpu_profile --example generate_pinned -- vmm_core/cpu_profile/profiles/<id>.json... > vmm_core/cpu_profile/src/pinned_data.rs
//! cargo xtask fmt --pass rustfmt --fix
//! ```
//!
//! List the files in catalog order. The crate's tests check every generated
//! constant against the files and the golden digests.

use cpu_profile::CpuProfile;
use cpu_profile::derive::KNOWN_GENERATIONS;
use std::fmt::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(source) => {
            print!("{source}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, String> {
    let mut profiles = Vec::new();
    for path in std::env::args().skip(1) {
        let json = std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))?;
        let profile =
            CpuProfile::from_pretty_json(&json).map_err(|error| format!("{path}: {error}"))?;
        profiles.push(profile);
    }
    if profiles.is_empty() {
        return Err("no profile files given".to_owned());
    }
    let mut out = String::new();
    writeln!(
        out,
        "// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The pinned profiles as static data, generated from `profiles/<id>.json` by
//! the `generate_pinned` example. Do not edit: the tests check every constant
//! against the JSON files and the golden digests.

use crate::derive::KNOWN_GENERATIONS;
use crate::pinned::PinnedProfile;
use crate::pinned::digest;
use crate::pinned::leaf;
use crate::pinned::msr;
use crate::pinned::source;
use crate::pinned::xsave;

/// The pinned profiles, in catalog order. Released profiles are immutable: a
/// change is a new revision with a new ID.
pub(crate) const PINNED: [PinnedProfile; {}] = [",
        profiles.len()
    )
    .unwrap();
    for profile in &profiles {
        render(&mut out, profile)?;
    }
    out.push_str("];\n");
    Ok(out)
}

fn render(out: &mut String, profile: &CpuProfile) -> Result<(), String> {
    let generation = KNOWN_GENERATIONS
        .iter()
        .position(|known| known.name == profile.generation().name)
        .ok_or_else(|| format!("{}: unknown generation", profile.id()))?;
    let encoding = String::from_utf8(profile.encode()).expect("canonical JSON is UTF-8");
    let hashes = (0..)
        .find(|&count| !encoding.contains(&format!("\"{}", "#".repeat(count + 1))))
        .unwrap()
        + 1;
    let fence = "#".repeat(hashes);
    let digest = profile
        .digest()
        .iter()
        .fold(String::new(), |mut hex, byte| {
            write!(hex, "{byte:02x}").unwrap();
            hex
        });
    writeln!(out, "    PinnedProfile {{").unwrap();
    writeln!(out, "        id: {:?},", profile.id()).unwrap();
    writeln!(out, "        digest: digest({digest:?}),").unwrap();
    writeln!(out, "        encoding: r{fence}\"{encoding}\"{fence},").unwrap();
    writeln!(out, "        description: {:?},", profile.description()).unwrap();
    writeln!(out, "        generation: &KNOWN_GENERATIONS[{generation}],").unwrap();
    writeln!(out, "        cpuid: &[").unwrap();
    for entry in profile.cpuid() {
        let subleaf = match entry.subleaf {
            Some(subleaf) => format!("Some({})", hex(subleaf.0.into())),
            None => "None".to_owned(),
        };
        writeln!(
            out,
            "            leaf({}, {subleaf}, {}, {}),",
            hex(entry.leaf.0.into()),
            hex_array(entry.values()),
            hex_array(entry.masks())
        )
        .unwrap();
    }
    writeln!(out, "        ],").unwrap();
    writeln!(out, "        xcr0: {},", hex(profile.xcr0())).unwrap();
    writeln!(out, "        xss: {},", hex(profile.xss())).unwrap();
    writeln!(out, "        xsave_components: &[").unwrap();
    for component in profile.xsave_components() {
        writeln!(
            out,
            "            xsave({}, {}, {}, {}, {}, {}),",
            component.index,
            component.size,
            component.offset,
            component.supervisor,
            component.align64,
            component.xfd
        )
        .unwrap();
    }
    writeln!(out, "        ],").unwrap();
    writeln!(
        out,
        "        physical_address_width: {},",
        profile.physical_address_width()
    )
    .unwrap();
    writeln!(out, "        msrs: &[").unwrap();
    for msr in profile.msrs() {
        writeln!(
            out,
            "            msr({}, {}, {}),",
            hex(msr.index.0.into()),
            hex(msr.value.0),
            hex(msr.mask.0)
        )
        .unwrap();
    }
    writeln!(out, "        ],").unwrap();
    writeln!(
        out,
        "        provenance_method: {:?},",
        profile.provenance().method
    )
    .unwrap();
    writeln!(out, "        provenance_sources: &[").unwrap();
    for source in &profile.provenance().sources {
        writeln!(
            out,
            "            source({:?}, {}, &{:?}),",
            source.backend, source.hosts, source.surface_digests
        )
        .unwrap();
    }
    writeln!(out, "        ],").unwrap();
    writeln!(out, "    }},").unwrap();
    Ok(())
}

/// Returns `value` as a Rust hex literal, with digit groups of four.
fn hex(value: u64) -> String {
    let digits = format!("{value:x}");
    if digits.len() <= 4 {
        return format!("0x{digits}");
    }
    let mut grouped = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i != 0 && (digits.len() - i) % 4 == 0 {
            grouped.push('_');
        }
        grouped.push(digit);
    }
    format!("0x{grouped}")
}

fn hex_array(values: [u32; 4]) -> String {
    format!("[{}]", values.map(|value| hex(value.into())).join(", "))
}
