// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The time ABI verification mode for host qualification
//! (`--x-time-abi-verify`): the VM worker builds the microVM partition and
//! runs the time ABI preflight, and the controller prints one
//! `NVX-TIME-ABI-VERIFY:` line and exits without running the guest.

use anyhow::Context;
use inspect::Inspect;
use mesh::CancelContext;
use mesh_worker::WorkerHandle;
use std::time::Duration;

/// The prefix of the verification line.
const PREFIX: &str = "NVX-TIME-ABI-VERIFY:";
/// The longest failure detail the verification line carries.
const MAX_DETAIL_LEN: usize = 400;
/// How long the controller waits for the worker's time ABI report.
const REPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Reports the outcome of a verification-mode worker launch: prints the
/// verification line and returns the process exit status, 0 when the time
/// ABI preflight passed and 1 when it failed.
pub(crate) async fn report_time_abi_verification(
    launched: anyhow::Result<WorkerHandle>,
    hypervisor: &str,
) -> anyhow::Result<i32> {
    let mut worker = match launched {
        Ok(worker) => worker,
        Err(err) => {
            tracing::error!(error = format!("{err:#}"), "time ABI verification failed");
            println!("{}", failure_line(hypervisor, &err));
            return Ok(1);
        }
    };
    let mut inspection = inspect::InspectionBuilder::new("time-abi")
        .depth(Some(1))
        .inspect(inspect::adhoc(|req| worker.inspect(req)));
    let resolved = CancelContext::new()
        .with_timeout(REPORT_TIMEOUT)
        .until_cancelled(inspection.resolve())
        .await;
    let report = inspection.results();
    let line = resolved
        .context("timed out waiting for the worker's time ABI report")
        .and_then(|_| success_line(hypervisor, &report));
    worker.stop();
    let stopped = worker
        .join()
        .await
        .context("VM worker failed after time ABI verification");
    let result = match (line, stopped) {
        (Ok(line), Ok(())) => Ok(line),
        (Err(err), Ok(())) | (Ok(_), Err(err)) => Err(err),
        (Err(err), Err(stopped)) => {
            Err(err.context(format!("worker shutdown also failed: {stopped:#}")))
        }
    };
    match result {
        Ok(line) => {
            println!("{line}");
            Ok(0)
        }
        Err(err) => {
            tracing::error!(error = format!("{err:#}"), "time ABI verification failed");
            println!("{}", failure_line(hypervisor, &err));
            Ok(1)
        }
    }
}

/// Returns the string value of `name` in the worker's `time-abi` inspect
/// node.
fn report_field(node: &inspect::Node, name: &str) -> anyhow::Result<String> {
    let inspect::Node::Dir(entries) = node else {
        anyhow::bail!("the VM worker returned no time ABI report: {node:?}");
    };
    let entry = entries
        .iter()
        .find(|entry| entry.name == name)
        .with_context(|| format!("the time ABI report lacks '{name}'"))?;
    match &entry.node {
        inspect::Node::Value(value) => Ok(match &value.kind {
            inspect::ValueKind::String(text) => text.clone(),
            _ => value.to_string(),
        }),
        node => anyhow::bail!("the time ABI report field '{name}' is {node:?}"),
    }
}

fn success_line(hypervisor: &str, node: &inspect::Node) -> anyhow::Result<String> {
    #[cfg(guest_arch = "x86_64")]
    {
        x86_success_line(hypervisor, node)
    }
    #[cfg(guest_arch = "aarch64")]
    {
        arm_success_line(hypervisor, node)
    }
}

/// Formats the line of a passed verification from the worker's `time-abi`
/// inspect node.
#[cfg(any(guest_arch = "x86_64", test))]
fn x86_success_line(hypervisor: &str, node: &inspect::Node) -> anyhow::Result<String> {
    let field = |name: &str| report_field(node, name);
    Ok(format!(
        "{PREFIX} v=1 status=ok backend={hypervisor} cpu_profile={} tsc_hz={} native_tsc_hz={} lapic_hz={} msr_route={} sync={}",
        field("cpu_profile")?,
        field("tsc_hz")?,
        field("native_tsc_hz")?,
        field("apic_hz")?,
        field("msr_route")?,
        field("sync")?,
    ))
}

#[cfg(any(guest_arch = "aarch64", test))]
fn arm_success_line(hypervisor: &str, node: &inspect::Node) -> anyhow::Result<String> {
    let field = |name: &str| report_field(node, name);
    let number = |name: &str| -> anyhow::Result<u64> {
        field(name)?
            .parse()
            .with_context(|| format!("invalid ARM time ABI report field '{name}'"))
    };
    let cpu_profile = field("cpu_profile")?;
    let digest = field("cpu_profile_sha256")?;
    anyhow::ensure!(
        !cpu_profile.is_empty()
            && cpu_profile.len() <= 32
            && cpu_profile.bytes().all(|c| c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || matches!(c, b'-' | b'_' | b'.'))
            && !matches!(cpu_profile.as_str(), "auto" | "host"),
        "invalid ARM CPU profile in the time ABI report"
    );
    anyhow::ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            && digest.bytes().any(|c| c != b'0'),
        "invalid ARM CPU profile digest in the time ABI report"
    );
    let counter_hz = number("counter_hz")?;
    let native_gpa_bits = number("backend_usable_gpa_bits")?;
    let configured_gpa_bits = number("configured_gpa_bits")?;
    let architectural_pa_bits = number("architectural_pa_bits")?;
    let effective_parange = number("effective_parange")?;
    let feature_bank_count = number("feature_bank_count")?;
    let feature_bank0 = number("feature_bank0")?;
    let feature_bank1 = number("feature_bank1")?;
    let cpu_id_registers_verified = number("cpu_id_registers_verified")?;
    anyhow::ensure!(
        counter_hz > 0
            && (1..=64).contains(&native_gpa_bits)
            && (1..=64).contains(&configured_gpa_bits)
            && configured_gpa_bits == architectural_pa_bits
            && effective_parange <= 6
            && feature_bank_count == 2
            && cpu_id_registers_verified > 0
            && field("backend_readback")? == "true"
            && field("effective_cpu")? == "true",
        "incomplete ARM CPU/time admission evidence"
    );
    Ok(format!(
        "{PREFIX} v=1 status=ok backend={hypervisor} architecture=aarch64 cpu_profile={cpu_profile} cpu_profile_sha256={digest} counter_hz={counter_hz} backend_usable_gpa_bits={native_gpa_bits} configured_gpa_bits={configured_gpa_bits} architectural_pa_bits={architectural_pa_bits} effective_parange={effective_parange} feature_bank_count={feature_bank_count} feature_bank0={feature_bank0} feature_bank1={feature_bank1} cpu_id_registers_verified={cpu_id_registers_verified} backend_readback=verified effective_cpu=verified guest_started=false"
    ))
}

/// Formats the line of a failed verification: the stable time ABI code in
/// the error chain, or `none`, and the escaped error chain.
fn failure_line(hypervisor: &str, err: &anyhow::Error) -> String {
    let detail = format!("{err:#}");
    let code = stable_code(&detail).unwrap_or("none");
    let mut escaped = String::new();
    for c in detail.chars() {
        if escaped.len() >= MAX_DETAIL_LEN {
            escaped.push_str("...");
            break;
        }
        match c {
            '"' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            c if c.is_control() => escaped.push(' '),
            c => escaped.push(c),
        }
    }
    format!("{PREFIX} v=1 status=fail backend={hypervisor} code={code} detail=\"{escaped}\"")
}

/// Formats the process's fatal-error message. A failure with a stable code,
/// such as a time ABI failure or a port forward that cannot bind, leads with
/// its code wherever the error chain carries it, so callers can match the
/// start of the message.
pub(crate) fn fatal_error_message(err: &anyhow::Error) -> String {
    let chain = format!("{err:#}");
    match stable_code(&chain) {
        Some(code) if !chain.starts_with(&format!("[{code}]")) => {
            format!("fatal error: [{code}] {err:?}")
        }
        _ => format!("fatal error: {err:?}"),
    }
}

/// Returns the first bracketed stable code, such as `E_TSC_SYNC_UNSUPPORTED`
/// or `E_PORT_FORWARD_BIND`, in an error chain.
fn stable_code(detail: &str) -> Option<&str> {
    let mut rest = detail;
    while let Some(start) = rest.find("[E_") {
        let candidate = &rest[start + 1..];
        if let Some(end) = candidate.find(']') {
            let code = &candidate[..end];
            if code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            {
                return Some(code);
            }
        }
        rest = candidate;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    fn entry(name: &str, value: impl Into<inspect::Value>) -> inspect::Entry {
        inspect::Entry {
            name: name.to_owned(),
            node: inspect::Node::Value(value.into()),
            sensitivity: Default::default(),
        }
    }

    #[test]
    fn success_line_reports_the_preflight() {
        let node = inspect::Node::Dir(vec![
            entry("tsc_hz", 2_100_000_000_u64),
            entry("apic_hz", 200_000_000_u64),
            entry("tsc_invariant_control", 0_u64),
            entry("hypervisor", "mshv".to_owned()),
            entry("cpu_profile", "intel.skylake-sp.v1".to_owned()),
            entry("msr_route", "ExitToVmm".to_owned()),
            entry("sync", "FrozenWrite".to_owned()),
            entry("native_tsc_hz", 2_100_000_000_u64),
        ]);
        assert_eq!(
            x86_success_line("mshv", &node).unwrap(),
            "NVX-TIME-ABI-VERIFY: v=1 status=ok backend=mshv cpu_profile=intel.skylake-sp.v1 tsc_hz=2100000000 native_tsc_hz=2100000000 lapic_hz=200000000 msr_route=ExitToVmm sync=FrozenWrite"
        );
        let inspect::Node::Dir(mut entries) = node else {
            unreachable!()
        };
        entries.retain(|entry| entry.name != "sync");
        let err = x86_success_line("mshv", &inspect::Node::Dir(entries)).unwrap_err();
        assert!(err.to_string().contains("'sync'"), "{err}");
        assert!(x86_success_line("mshv", &inspect::Node::Unevaluated).is_err());
    }

    #[test]
    fn arm_success_requires_complete_native_admission_evidence() {
        let entries = vec![
            entry("cpu_profile", "qualcomm.oryon-x1.v1".to_owned()),
            entry("cpu_profile_sha256", "5a".repeat(32)),
            entry("counter_hz", 19_200_000_u64),
            entry("backend_usable_gpa_bits", 39_u32),
            entry("configured_gpa_bits", 36_u32),
            entry("architectural_pa_bits", 36_u32),
            entry("effective_parange", 1_u32),
            entry("feature_bank_count", 2_u32),
            entry("feature_bank0", 123_u64),
            entry("feature_bank1", 456_u64),
            entry("cpu_id_registers_verified", 15_u32),
            entry("backend_readback", true),
            entry("effective_cpu", true),
        ];
        let line = arm_success_line("whp", &inspect::Node::Dir(entries.clone())).unwrap();
        assert_eq!(
            line,
            format!(
                "NVX-TIME-ABI-VERIFY: v=1 status=ok backend=whp architecture=aarch64 cpu_profile=qualcomm.oryon-x1.v1 cpu_profile_sha256={} counter_hz=19200000 backend_usable_gpa_bits=39 configured_gpa_bits=36 architectural_pa_bits=36 effective_parange=1 feature_bank_count=2 feature_bank0=123 feature_bank1=456 cpu_id_registers_verified=15 backend_readback=verified effective_cpu=verified guest_started=false",
                "5a".repeat(32),
            )
        );
        for missing in 0..entries.len() {
            let mut partial = entries.clone();
            partial.remove(missing);
            assert!(arm_success_line("whp", &inspect::Node::Dir(partial)).is_err());
        }
        for invalid in [
            entry("counter_hz", 0_u64),
            entry("cpu_profile_sha256", "00".repeat(32)),
            entry("cpu_id_registers_verified", 0_u32),
            entry("configured_gpa_bits", 40_u32),
            entry("backend_readback", false),
            entry("effective_cpu", false),
        ] {
            let mut bad = entries.clone();
            let index = bad.iter().position(|e| e.name == invalid.name).unwrap();
            bad[index] = invalid;
            assert!(arm_success_line("whp", &inspect::Node::Dir(bad)).is_err());
        }
        assert!(arm_success_line("whp", &inspect::Node::Unevaluated).is_err());
    }

    #[test]
    fn failure_line_carries_the_code_and_escaped_detail() {
        let err = anyhow::anyhow!("[E_TSC_SYNC_UNSUPPORTED] no \"sync\"\nset")
            .context("failed to launch vm worker");
        assert_eq!(
            failure_line("kvm", &err),
            "NVX-TIME-ABI-VERIFY: v=1 status=fail backend=kvm code=E_TSC_SYNC_UNSUPPORTED detail=\"failed to launch vm worker: [E_TSC_SYNC_UNSUPPORTED] no \\\"sync\\\" set\""
        );
        let err = anyhow::anyhow!("kvm device unavailable [E_lower] x");
        assert!(failure_line("kvm", &err).contains(" code=none "));
        let long = anyhow::anyhow!("{}", "x".repeat(1000));
        assert!(failure_line("kvm", &long).ends_with("...\""));
    }

    #[test]
    fn fatal_error_message_leads_with_the_time_abi_code() {
        // `{err:?}` appends a backtrace when RUST_BACKTRACE or
        // RUST_LIB_BACKTRACE enables capture, so compare whole messages with
        // the same rendering and leading lines with literals.
        let err = anyhow::anyhow!("[E_TSC_SYNC_UNSUPPORTED] no synchronized set")
            .context("failed to create the partition")
            .context("failed to launch vm worker");
        let message = fatal_error_message(&err);
        assert_eq!(
            message,
            format!("fatal error: [E_TSC_SYNC_UNSUPPORTED] {err:?}")
        );
        assert_eq!(
            message.lines().next(),
            Some("fatal error: [E_TSC_SYNC_UNSUPPORTED] failed to launch vm worker")
        );

        let err = anyhow::anyhow!("[E_TEST_HOOK] unknown hook");
        let message = fatal_error_message(&err);
        assert_eq!(message, format!("fatal error: {err:?}"));
        assert_eq!(
            message.lines().next(),
            Some("fatal error: [E_TEST_HOOK] unknown hook")
        );

        let err = anyhow::anyhow!("no code here");
        let message = fatal_error_message(&err);
        assert_eq!(message, format!("fatal error: {err:?}"));
        assert_eq!(message.lines().next(), Some("fatal error: no code here"));
    }

    #[test]
    fn fatal_error_message_leads_with_the_code_of_a_port_forward_that_cannot_bind() {
        // The resolver of the VM's NIC fails the launch of the VM worker, as
        // at a restore that installs a new network policy.
        let err = anyhow::anyhow!(
            "[E_PORT_FORWARD_BIND] failed to create socket for port forward (Tcp 127.0.0.1:18080)"
        )
        .context("failed to resolve the network endpoint")
        .context("failed to launch vm worker");
        let message = fatal_error_message(&err);
        assert_eq!(
            message,
            format!("fatal error: [E_PORT_FORWARD_BIND] {err:?}")
        );
        assert_eq!(
            message.lines().next(),
            Some("fatal error: [E_PORT_FORWARD_BIND] failed to launch vm worker")
        );
    }
}
