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
    let _ = CancelContext::new()
        .with_timeout(REPORT_TIMEOUT)
        .until_cancelled(inspection.resolve())
        .await;
    let report = inspection.results();
    let line = success_line(hypervisor, &report)?;
    worker.stop();
    worker
        .join()
        .await
        .context("VM worker failed after time ABI verification")?;
    println!("{line}");
    Ok(0)
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

/// Formats the line of a passed verification from the worker's `time-abi`
/// inspect node.
fn success_line(hypervisor: &str, node: &inspect::Node) -> anyhow::Result<String> {
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

/// Formats the line of a failed verification: the stable time ABI code in
/// the error chain, or `none`, and the escaped error chain.
fn failure_line(hypervisor: &str, err: &anyhow::Error) -> String {
    let detail = format!("{err:#}");
    let code = time_abi_code(&detail).unwrap_or("none");
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

/// Formats the process's fatal-error message. A time ABI failure leads with
/// its stable code wherever the error chain carries it, so callers can match
/// the start of the message.
pub(crate) fn fatal_error_message(err: &anyhow::Error) -> String {
    let chain = format!("{err:#}");
    match time_abi_code(&chain) {
        Some(code) if !chain.starts_with(&format!("[{code}]")) => {
            format!("fatal error: [{code}] {err:?}")
        }
        _ => format!("fatal error: {err:?}"),
    }
}

/// Returns the first bracketed time ABI code, such as `E_TSC_SYNC_UNSUPPORTED`,
/// in an error chain.
fn time_abi_code(detail: &str) -> Option<&str> {
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
            success_line("mshv", &node).unwrap(),
            "NVX-TIME-ABI-VERIFY: v=1 status=ok backend=mshv cpu_profile=intel.skylake-sp.v1 tsc_hz=2100000000 native_tsc_hz=2100000000 lapic_hz=200000000 msr_route=ExitToVmm sync=FrozenWrite"
        );
        let inspect::Node::Dir(mut entries) = node else {
            unreachable!()
        };
        entries.retain(|entry| entry.name != "sync");
        let err = success_line("mshv", &inspect::Node::Dir(entries)).unwrap_err();
        assert!(err.to_string().contains("'sync'"), "{err}");
        assert!(success_line("mshv", &inspect::Node::Unevaluated).is_err());
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
        let err = anyhow::anyhow!("[E_TSC_SYNC_UNSUPPORTED] no synchronized set")
            .context("failed to create the partition")
            .context("failed to launch vm worker");
        let message = fatal_error_message(&err);
        assert!(
            message.starts_with("fatal error: [E_TSC_SYNC_UNSUPPORTED] failed to launch vm worker"),
            "{message}"
        );

        let err = anyhow::anyhow!("[E_TEST_HOOK] unknown hook");
        let message = fatal_error_message(&err);
        assert_eq!(message, "fatal error: [E_TEST_HOOK] unknown hook");

        let err = anyhow::anyhow!("no code here");
        assert_eq!(fatal_error_message(&err), "fatal error: no code here");
    }
}
