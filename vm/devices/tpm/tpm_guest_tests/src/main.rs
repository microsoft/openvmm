// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line utility for interacting with a physical TPM during guest attestation tests.
//! Supports reading the AK certificate NV index and producing attestation reports with
//! optional user-provided payloads.

mod tpm;

use std::env;
use std::error::Error;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tpm_lib::TpmEngine;
use tpm_lib::TpmEngineHelper;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;

use tpm::Tpm;

const NV_INDEX_AK_CERT: u32 = tpm_protocol::TPM_NV_INDEX_AIK_CERT;
const NV_INDEX_ATTESTATION_REPORT: u32 = tpm_protocol::TPM_NV_INDEX_ATTESTATION_REPORT;
const NV_INDEX_GUEST_INPUT: u32 = tpm_protocol::TPM_NV_INDEX_GUEST_ATTESTATION_INPUT;

const MAX_NV_READ_SIZE: usize = 4096;
const MAX_ATTESTATION_READ_SIZE: usize = 2600;
const GUEST_INPUT_SIZE: u16 = 64;
const GUEST_INPUT_AUTH: u64 = 0;
const AK_CERT_RETRY_DELAY_MS: u64 = 200;

#[derive(Debug, Default)]
struct Config {
    ak_cert: bool,
    ak_cert_expected: Option<Vec<u8>>,
    ak_cert_retry_attempts: u32,
    report: bool,
    user_data: Option<Vec<u8>>,
}

enum ArgsOutcome {
    Config(Config),
    Help,
    Error(String),
}

fn main() {
    match parse_args(env::args()) {
        ArgsOutcome::Help => {
            print_usage();
        }
        ArgsOutcome::Error(message) => {
            eprintln!("error: {message}");
            eprintln!();
            print_usage();
            std::process::exit(1);
        }
        ArgsOutcome::Config(config) => {
            if let Err(err) = run(&config) {
                eprintln!("error: {}", err);
                let mut source = err.source();
                while let Some(inner) = source {
                    eprintln!("caused by: {}", inner);
                    source = inner.source();
                }
                std::process::exit(1);
            }
        }
    }
}

fn run(config: &Config) -> Result<(), Box<dyn Error>> {
    println!("Connecting to physical TPM device…");
    let tpm = Tpm::open_default()?;
    let mut helper = tpm.into_engine_helper();

    if config.ak_cert {
        handle_ak_cert(
            &mut helper,
            config.ak_cert_expected.as_deref(),
            config.ak_cert_retry_attempts,
        )?;
    }

    if config.report {
        let payload = build_guest_input_payload(config.user_data.as_deref())?;
        handle_report(&mut helper, &payload)?;
    }

    Ok(())
}

fn handle_ak_cert<E: TpmEngine>(
    helper: &mut TpmEngineHelper<E>,
    expected: Option<&[u8]>,
    retry_attempts: u32,
) -> Result<(), Box<dyn Error>> {
    for attempt in 0..=retry_attempts {
        if attempt > 0 {
            println!(
                "AK certificate mismatch; retrying after {} ms ({}/{})…",
                AK_CERT_RETRY_DELAY_MS,
                attempt,
                retry_attempts
            );
            thread::sleep(Duration::from_millis(AK_CERT_RETRY_DELAY_MS));
        }

        println!("Reading AK certificate from NV index {NV_INDEX_AK_CERT:#x}…");
        let data = read_nv_index(helper, NV_INDEX_AK_CERT)?;

        if data.len() > MAX_NV_READ_SIZE {
            return Err(format!(
                "AK certificate size {} exceeds maximum {} bytes",
                data.len(),
                MAX_NV_READ_SIZE
            )
            .into());
        }

        print_nv_summary("AK certificate", &data);

        if let Some(expected) = expected {
            if data == expected {
                println!(
                    "AK certificate matches expected value ({} bytes).",
                    data.len()
                );
                return Ok(());
            }

            if attempt == retry_attempts {
                return Err("AK certificate contents did not match expected value".into());
            }
        } else {
            return Ok(());
        }
    }

    unreachable!("loop must exit via success or error");
}

fn handle_report<E: TpmEngine>(
    helper: &mut TpmEngineHelper<E>,
    payload: &[u8],
) -> Result<(), Box<dyn Error>> {
    ensure_guest_input_index(helper)?;

    println!(
        "Writing {} bytes of guest attestation input to NV index {NV_INDEX_GUEST_INPUT:#x}…",
        payload.len()
    );
    helper.nv_write(TPM20_RH_OWNER, None, NV_INDEX_GUEST_INPUT, &payload)?;

    let guest_data = read_nv_index(helper, NV_INDEX_GUEST_INPUT)?;
    print_nv_summary("Guest attestation input", &guest_data);

    println!("Reading attestation report from NV index {NV_INDEX_ATTESTATION_REPORT:#x}…");
    let att_report = read_nv_index(helper, NV_INDEX_ATTESTATION_REPORT)?;

    if att_report.len() > MAX_ATTESTATION_READ_SIZE {
        return Err(format!(
            "attestation report size {} exceeds maximum {} bytes",
            att_report.len(),
            MAX_ATTESTATION_READ_SIZE
        )
        .into());
    }

    print_nv_summary("Attestation report", &att_report);

    Ok(())
}

fn ensure_guest_input_index<E: TpmEngine>(
    helper: &mut TpmEngineHelper<E>,
) -> Result<(), Box<dyn Error>> {
    if helper.find_nv_index(NV_INDEX_GUEST_INPUT)?.is_some() {
        return Ok(());
    };

    println!(
        "NV index {NV_INDEX_GUEST_INPUT:#x} not defined; allocating {} bytes…",
        GUEST_INPUT_SIZE
    );

    helper
        .nv_define_space(
            TPM20_RH_OWNER,
            GUEST_INPUT_AUTH,
            NV_INDEX_GUEST_INPUT,
            GUEST_INPUT_SIZE,
        )
        .map_err(|e| -> Box<dyn Error> { Box::new(e) })?;

    Ok(())
}

fn read_nv_index<E: TpmEngine>(
    helper: &mut TpmEngineHelper<E>,
    nv_index: u32,
) -> Result<Vec<u8>, Box<dyn Error>> {
    let Some(res) = helper.find_nv_index(nv_index)? else {
        // nv index may not exist before guest makes a request
        return Err(format!("NV index {nv_index:#x} not found").into());
    };
    let nv_index_size = res.nv_public.nv_public.data_size.get();
    let mut buffer = vec![0u8; nv_index_size as usize];
    helper.nv_read(TPM20_RH_OWNER, nv_index, nv_index_size, &mut buffer)?;

    Ok(buffer)
}

fn build_guest_input_payload(user_data: Option<&[u8]>) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut payload = vec![0u8; GUEST_INPUT_SIZE as usize];

    if let Some(data) = user_data {
        if data.len() > payload.len() {
            return Err(format!(
                "user data length {} exceeds {} byte guest input size",
                data.len(),
                payload.len()
            )
            .into());
        }
        payload[..data.len()].copy_from_slice(data);
        Ok(payload)
    } else {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let message = format!("tpm_guest_tests {:016x}", timestamp);
        let copy_len = message.len().min(payload.len());
        payload[..copy_len].copy_from_slice(&message.as_bytes()[..copy_len]);

        Ok(payload)
    }
}

fn parse_args<I>(args: I) -> ArgsOutcome
where
    I: IntoIterator<Item = String>,
{
    let mut iter = args.into_iter();
    // Skip program name
    iter.next();

    let mut iter = iter.peekable();
    let mut config = Config::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--ak-cert" => {
                config.ak_cert = true;
            }
            "--report" => {
                config.report = true;
            }
            "--expected-data" => {
                if config.ak_cert_expected.is_some() {
                    return ArgsOutcome::Error(
                        "--expected-data or --expected-data-hex specified multiple times".into(),
                    );
                }
                let value = match iter.next() {
                    Some(v) => v.into_bytes(),
                    None => {
                        return ArgsOutcome::Error("--expected-data requires an argument".into());
                    }
                };
                config.ak_cert_expected = Some(value);
            }
            "--expected-data-hex" => {
                if config.ak_cert_expected.is_some() {
                    return ArgsOutcome::Error(
                        "--expected-data or --expected-data-hex specified multiple times".into(),
                    );
                }
                let value = match iter.next() {
                    Some(v) => v,
                    None => {
                        return ArgsOutcome::Error(
                            "--expected-data-hex requires an argument".into(),
                        );
                    }
                };
                match parse_hex_bytes(&value) {
                    Ok(bytes) => config.ak_cert_expected = Some(bytes),
                    Err(e) => return ArgsOutcome::Error(e),
                }
            }
            "--retry" => {
                if config.ak_cert_retry_attempts != 0 {
                    return ArgsOutcome::Error("--retry specified multiple times".into());
                }
                let value = match iter.next() {
                    Some(v) => v,
                    None => return ArgsOutcome::Error("--retry requires an argument".into()),
                };
                let retries = match value.parse::<u32>() {
                    Ok(0) => {
                        return ArgsOutcome::Error("--retry requires a positive integer".into());
                    }
                    Ok(n) => n,
                    Err(_) => {
                        return ArgsOutcome::Error(format!(
                            "invalid retry count '{value}'"
                        ));
                    }
                };
                config.ak_cert_retry_attempts = retries;
            }
            "--user-data" => {
                if config.user_data.is_some() {
                    return ArgsOutcome::Error("--user-data specified multiple times".into());
                }
                let value = match iter.next() {
                    Some(v) => v.into_bytes(),
                    None => return ArgsOutcome::Error("--user-data requires an argument".into()),
                };
                config.user_data = Some(value);
            }
            "--user-data-hex" => {
                if config.user_data.is_some() {
                    return ArgsOutcome::Error(
                        "--user-data or --user-data-hex specified multiple times".into(),
                    );
                }
                let value = match iter.next() {
                    Some(v) => v,
                    None => {
                        return ArgsOutcome::Error("--user-data-hex requires an argument".into());
                    }
                };
                match parse_hex_bytes(&value) {
                    Ok(bytes) => config.user_data = Some(bytes),
                    Err(e) => return ArgsOutcome::Error(e),
                }
            }
            "--help" | "-h" => return ArgsOutcome::Help,
            other => {
                return ArgsOutcome::Error(format!("unrecognized argument '{other}'"));
            }
        }
    }

    if config.user_data.is_some() && !config.report {
        return ArgsOutcome::Error("--user-data requires --report".into());
    }

    if config.ak_cert_expected.is_some() && !config.ak_cert {
        return ArgsOutcome::Error("--expected-data requires --ak-cert".into());
    }

    if config.ak_cert_retry_attempts > 0 && !config.ak_cert {
        return ArgsOutcome::Error("--retry requires --ak-cert".into());
    }

    if config.ak_cert_retry_attempts > 0 && config.ak_cert_expected.is_none() {
        return ArgsOutcome::Error("--retry requires expected AK certificate data".into());
    }

    if !config.ak_cert && !config.report {
        return ArgsOutcome::Error("no action specified".into());
    }

    ArgsOutcome::Config(config)
}

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>, String> {
    let trimmed = value.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);

    if hex.len() % 2 != 0 {
        return Err("hex data must contain an even number of characters".into());
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let chars: Vec<char> = hex.chars().collect();
    for chunk in chars.chunks(2) {
        let hi = chunk[0]
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex character '{}'", chunk[0]))?;
        let lo = chunk[1]
            .to_digit(16)
            .ok_or_else(|| format!("invalid hex character '{}'", chunk[1]))?;
        bytes.push(((hi << 4) | lo) as u8);
    }

    Ok(bytes)
}

fn print_usage() {
    println!("Usage: tpm_guest_tests [OPTIONS]\n");
    println!("Options:");
    println!("  --ak-cert                 Read the AK certificate NV index");
    println!("      --expected-data <utf8>   Compare AK certificate to provided UTF-8 data");
    println!("      --expected-data-hex <0x..> Compare AK certificate to provided hex data");
    println!(
        "      --retry <times>          Retry AK certificate comparison up to <times> times"
    );
    println!("  --report                  Write guest input and read the attestation report");
    println!(
        "  --user-data <text>        Provide UTF-8 user data for --report (max {} bytes)",
        GUEST_INPUT_SIZE
    );
    println!(
        "  --user-data-hex <hex>     Provide hex-encoded user data for --report (max {} bytes)",
        GUEST_INPUT_SIZE
    );
    println!("  -h, --help                Show this help message");
}

fn print_nv_summary(label: &str, data: &[u8]) {
    println!("{label}");
    if data.is_empty() {
        println!("{label} data: <empty>");
        return;
    }

    println!("{label} data ({} bytes):", data.len());
    hexdump(data, 256);
    if data.len() > 256 {
        println!(
            "… {} additional bytes not shown (total {} bytes)",
            data.len() - 256,
            data.len()
        );
    }
}

fn hexdump(data: &[u8], limit: usize) {
    let max = data.len().min(limit);
    for (row, chunk) in data[..max].chunks(16).enumerate() {
        print!("{:04x}: ", row * 16);
        let mut ascii = String::new();
        for byte in chunk {
            print!("{:02x} ", byte);
            let ch = if (0x20..=0x7e).contains(byte) {
                *byte as char
            } else {
                '.'
            };
            ascii.push(ch);
        }
        for _ in chunk.len()..16 {
            print!("   ");
        }
        println!(" |{}|", ascii);
    }
}
