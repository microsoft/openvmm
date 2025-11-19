// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Command-line utility for interacting with a physical TPM during guest attestation tests.
//! Supports reading the AK certificate NV index and producing attestation reports with
//! optional user-provided payloads.

mod report;
mod tpm;

use std::error::Error;
use std::io;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use clap::Args;
use clap::Parser;
use clap::Subcommand;
use serde_json::Value;
use zerocopy::FromBytes;

use tpm_lib::TpmEngine;
use tpm_lib::TpmEngineHelper;
use tpm_protocol::tpm20proto::TPM20_RH_OWNER;

use report::IGVM_ATTEST_REQUEST_VERSION_1;
use report::IGVM_ATTESTATION_SIGNATURE;
use report::IGVM_ATTESTATION_VERSION;
use report::IGVM_REQUEST_BASE_SIZE;
use report::IGVM_REQUEST_DATA_OFFSET;
use report::IGVM_REQUEST_DATA_SIZE;
use report::IGVM_REQUEST_TYPE_AK_CERT;
use report::IgvmAttestRequestData;
use report::IgvmAttestRequestHeader;
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
    show_runtime_claims: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = "tpm_guest_tests",
    about = "Guest attestation TPM helper utility",
    version,
    long_about = None,
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Read the AK certificate NV index
    #[command(name = "ak_cert")]
    AkCert(AkCertArgs),
    /// Write guest input and read the attestation report
    #[command(name = "report")]
    Report(ReportArgs),
}

#[derive(Args, Debug, Default)]
struct AkCertArgs {
    /// Expected AK certificate contents (UTF-8)
    #[arg(long, value_name = "UTF8", conflicts_with = "expected_data_hex")]
    expected_data: Option<String>,

    /// Expected AK certificate contents (hex)
    #[arg(long, value_name = "HEX", conflicts_with = "expected_data")]
    expected_data_hex: Option<String>,

    /// Retry AK certificate comparison up to COUNT times
    #[arg(long, value_name = "COUNT", value_parser = clap::value_parser!(u32).range(1..))]
    retry: Option<u32>,
}

#[derive(Args, Debug, Default)]
struct ReportArgs {
    /// Guest attestation input payload (UTF-8)
    #[arg(long, value_name = "TEXT", conflicts_with = "user_data_hex")]
    user_data: Option<String>,

    /// Guest attestation input payload (hex)
    #[arg(long, value_name = "HEX", conflicts_with = "user_data")]
    user_data_hex: Option<String>,

    /// Decode and pretty-print runtime claims from attestation report
    #[arg(long)]
    show_runtime_claims: bool,
}

fn main() {
    let cli = Cli::parse();
    let config = match config_from_cli(cli) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    };

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

fn run(config: &Config) -> Result<(), Box<dyn Error>> {
    println!("Connecting to physical TPM device…");

    let tpm = Tpm::open()?;
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
        let att_report = handle_report(&mut helper, &payload)?;
        if config.show_runtime_claims {
            print_runtime_claims(&att_report)?;
        }
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
                AK_CERT_RETRY_DELAY_MS, attempt, retry_attempts
            );
            thread::sleep(Duration::from_millis(AK_CERT_RETRY_DELAY_MS));
        }

        println!("Reading AK certificate from NV index {NV_INDEX_AK_CERT:#x}…");
        let data = match read_nv_index(helper, NV_INDEX_AK_CERT) {
            Ok(data) => data,
            Err(err) => {
                if attempt == retry_attempts {
                    return Err(format!("Failed to read AK certificate: {err}").into());
                }
                // Allow retry on failure
                continue;
            }
        };

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

fn config_from_cli(cli: Cli) -> Result<Config, String> {
    let mut config = Config::default();

    match cli.command {
        Command::AkCert(args) => {
            config.ak_cert = true;

            if let Some(data) = args.expected_data {
                config.ak_cert_expected = Some(data.into_bytes());
            }

            if let Some(hex) = args.expected_data_hex {
                let bytes =
                    parse_hex_bytes(&hex).map_err(|e| format!("--expected-data-hex: {e}"))?;
                config.ak_cert_expected = Some(bytes);
            }

            if let Some(retry) = args.retry {
                if config.ak_cert_expected.is_none() {
                    return Err("--retry requires expected AK certificate data".into());
                }
                config.ak_cert_retry_attempts = retry;
            }
        }
        Command::Report(args) => {
            config.report = true;
            config.show_runtime_claims = args.show_runtime_claims;

            if let Some(data) = args.user_data {
                config.user_data = Some(data.into_bytes());
            }

            if let Some(hex) = args.user_data_hex {
                let bytes = parse_hex_bytes(&hex).map_err(|e| format!("--user-data-hex: {e}"))?;
                config.user_data = Some(bytes);
            }
        }
    }

    Ok(config)
}

fn handle_report<E: TpmEngine>(
    helper: &mut TpmEngineHelper<E>,
    payload: &[u8],
) -> Result<Vec<u8>, Box<dyn Error>> {
    ensure_guest_input_index(helper)?;

    println!(
        "Writing {} bytes of guest attestation input to NV index {NV_INDEX_GUEST_INPUT:#x}…",
        payload.len()
    );
    helper.nv_write(TPM20_RH_OWNER, None, NV_INDEX_GUEST_INPUT, payload)?;

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

    Ok(att_report)
}

fn print_runtime_claims(attestation_report: &[u8]) -> Result<(), Box<dyn Error>> {
    match runtime_claims_json(attestation_report)? {
        Some(json) => {
            let pretty = serde_json::to_string_pretty(&json)?;
            println!("Runtime claims JSON:");
            println!("{pretty}");
        }
        None => println!("Runtime claims: <empty>"),
    }

    Ok(())
}

fn runtime_claims_json(attestation_report: &[u8]) -> Result<Option<Value>, Box<dyn Error>> {
    if attestation_report.len() < IGVM_REQUEST_BASE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "attestation report length {} is smaller than base request size {}",
                attestation_report.len(),
                IGVM_REQUEST_BASE_SIZE
            ),
        )
        .into());
    }

    let (header, _) =
        IgvmAttestRequestHeader::read_from_prefix(attestation_report).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "failed to read attestation report header",
            )
        })?;

    if header.version != IGVM_ATTESTATION_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected header version {}", header.version),
        )
        .into());
    }

    if header.signature != IGVM_ATTESTATION_SIGNATURE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected attestation signature {:#x}", header.signature),
        )
        .into());
    }

    if header.request_type != IGVM_REQUEST_TYPE_AK_CERT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected attestation request type {}",
                header.request_type
            ),
        )
        .into());
    }

    let report_size = usize::try_from(header.report_size).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "reported attestation size does not fit platform usize",
        )
    })?;
    if report_size < IGVM_REQUEST_BASE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "reported attestation size {report_size} smaller than base request {}",
                IGVM_REQUEST_BASE_SIZE
            ),
        )
        .into());
    }
    if report_size > attestation_report.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "attestation report claims {report_size} bytes but only {} bytes provided",
                attestation_report.len()
            ),
        )
        .into());
    }

    let request_data_end = IGVM_REQUEST_DATA_OFFSET + IGVM_REQUEST_DATA_SIZE;
    if request_data_end > attestation_report.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attestation report truncated before request data",
        )
        .into());
    }

    let (request_data, _) = IgvmAttestRequestData::read_from_prefix(
        &attestation_report[IGVM_REQUEST_DATA_OFFSET..request_data_end],
    )
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "failed to read attestation request data",
        )
    })?;

    if request_data.version != IGVM_ATTEST_REQUEST_VERSION_1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected attestation request version {}",
                request_data.version
            ),
        )
        .into());
    }

    let runtime_claims_len = request_data.variable_data_size as usize;
    if runtime_claims_len == 0 {
        return Ok(None);
    }

    let expected_data_size = IGVM_REQUEST_DATA_SIZE + runtime_claims_len;
    if request_data.data_size as usize != expected_data_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attestation request data size mismatch",
        )
        .into());
    }

    let runtime_start = IGVM_REQUEST_BASE_SIZE;
    if runtime_start + runtime_claims_len != report_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime claims extend beyond attestation report",
        )
        .into());
    }

    if runtime_start + runtime_claims_len > attestation_report.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "attestation report buffer {} shorter than claimed runtime data {}",
                attestation_report.len(),
                runtime_claims_len
            ),
        )
        .into());
    }

    let runtime_bytes = &attestation_report[runtime_start..runtime_start + runtime_claims_len];
    if runtime_bytes.is_empty() {
        return Ok(None);
    }

    let json = serde_json::from_slice::<Value>(runtime_bytes).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse runtime claims JSON: {err}"),
        )
    })?;

    Ok(Some(json))
}

fn ensure_guest_input_index<E: TpmEngine>(
    helper: &mut TpmEngineHelper<E>,
) -> Result<(), Box<dyn Error>> {
    if helper.nv_read_public(NV_INDEX_GUEST_INPUT).is_ok() {
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
    let Ok(res) = helper.nv_read_public(nv_index) else {
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

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>, String> {
    let trimmed = value.trim();
    let hex = trimmed.strip_prefix("0x").unwrap_or(trimmed);

    if !hex.len().is_multiple_of(2) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::IgvmAttestRequestBase;
    use clap::Parser;
    use serde_json::json;
    use zerocopy::IntoBytes;

    #[test]
    fn runtime_claims_json_parses_version1() {
        let runtime_json = serde_json::to_vec(&json!({
            "keys": [],
            "vm-configuration": {
                "currentTime": 1_704_000_000,
                "rootCertThumbprint": "thumbprint",
                "consoleEnabled": true,
                "secureBoot": true,
                "tpmEnabled": true,
                "tpmPersisted": false,
                "filteredVpciDevicesAllowed": true,
                "vmUniqueId": "vm_id",
            },
            "user-data": "deadbeef",
        }))
        .expect("serialize runtime claims");

        let mut request = IgvmAttestRequestBase::default();
        request.header.signature = IGVM_ATTESTATION_SIGNATURE;
        request.header.version = IGVM_ATTESTATION_VERSION;
        request.header.report_size = (IGVM_REQUEST_BASE_SIZE + runtime_json.len()) as u32;
        request.header.request_type = IGVM_REQUEST_TYPE_AK_CERT;
        request.request_data.data_size = (IGVM_REQUEST_DATA_SIZE + runtime_json.len()) as u32;
        request.request_data.version = IGVM_ATTEST_REQUEST_VERSION_1;
        request.request_data.variable_data_size = runtime_json.len() as u32;

        let mut buffer = Vec::from(request.as_bytes());
        buffer.extend_from_slice(&runtime_json);

        let parsed = runtime_claims_json(&buffer)
            .expect("parse runtime claims")
            .expect("claims present");

        assert_eq!(parsed["vm-configuration"]["vmUniqueId"], "vm_id");
        assert_eq!(parsed["user-data"], "deadbeef");
    }

    #[test]
    fn runtime_claims_json_parses_unsupported_version() {
        let mut runtime_json = serde_json::to_vec(&json!({
            "keys": [],
            "vm-configuration": {
                "currentTime": 1_704_000_000,
                "rootCertThumbprint": "thumbprint",
                "consoleEnabled": true,
                "secureBoot": true,
                "tpmEnabled": true,
                "tpmPersisted": false,
                "filteredVpciDevicesAllowed": true,
                "vmUniqueId": "vm_id",
            },
            "user-data": "deadbeef",
        }))
        .expect("serialize runtime claims");
        runtime_json.extend_from_slice(&[0, 0]);

        let mut request = IgvmAttestRequestBase::default();
        request.header.signature = IGVM_ATTESTATION_SIGNATURE;
        request.header.version = IGVM_ATTESTATION_VERSION;
        request.header.report_size = (IGVM_REQUEST_BASE_SIZE + runtime_json.len()) as u32;
        request.header.request_type = IGVM_REQUEST_TYPE_AK_CERT;
        request.request_data.data_size = (IGVM_REQUEST_DATA_SIZE + runtime_json.len()) as u32;
        request.request_data.version = IGVM_ATTEST_REQUEST_VERSION_1 + 1;
        request.request_data.variable_data_size = runtime_json.len() as u32;

        let mut buffer = Vec::from(request.as_bytes());
        buffer.extend_from_slice(&runtime_json);

        runtime_claims_json(&buffer).expect_err("parsing unsupported version should fail");
    }

    #[test]
    fn cli_requires_action() {
        assert!(Cli::try_parse_from(["tpm_guest_tests"]).is_err());
    }

    #[test]
    fn cli_retry_requires_expected_data() {
        let cli =
            Cli::try_parse_from(["tpm_guest_tests", "ak_cert", "--retry", "2"]).expect("parse CLI");
        let err = config_from_cli(cli).expect_err("retry should require expected data");
        assert!(err.contains("--retry"));
    }

    #[test]
    fn cli_expected_data_hex_parses() {
        let cli = Cli::try_parse_from([
            "tpm_guest_tests",
            "ak_cert",
            "--expected-data-hex",
            "0x4142",
        ])
        .expect("parse CLI");
        let config = config_from_cli(cli).expect("build config");
        assert_eq!(config.ak_cert_expected.as_deref(), Some(&b"AB"[..]));
    }
}
