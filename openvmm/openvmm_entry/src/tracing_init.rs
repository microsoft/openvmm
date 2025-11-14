// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context as _;
use anyhow::anyhow;
use std::io::IsTerminal;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::format::Format;
use tracing_subscriber::fmt::time::uptime;

#[cfg(windows)]
const OPENVMM_PROVIDER_GUID: guid::Guid = guid::guid!("22bc55fe-2116-5adc-12fb-3fadfd7e360c");
#[cfg(windows)]
const OPENVMM_KEYWORD_TRACE_LEVEL: u64 = 0x1;

/// Reads an environment variable, falling back to a legacy variable (replacing
/// "OPENVMM_" with "HVLITE_") if the original is not set.
fn legacy_openvmm_env(name: &str) -> Result<String, std::env::VarError> {
    std::env::var(name).or_else(|_| {
        std::env::var(format!(
            "HVLITE_{}",
            name.strip_prefix("OPENVMM_").unwrap_or(name)
        ))
    })
}

/// Enables tracing output to stderr.
pub fn enable_tracing() -> anyhow::Result<()> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Enable tracing for paravisor_log by default since this is passed through
    // from the guest (but still allow it to be disabled via OPENVMM_LOG).
    let base = "paravisor_log=trace";
    let filter = if let Ok(filter) = legacy_openvmm_env("OPENVMM_LOG") {
        tracing_subscriber::EnvFilter::try_new(format!("{base},{filter}"))
            .context("invalid OPENVMM_LOG")?
    } else {
        tracing_subscriber::EnvFilter::default()
            .add_directive(tracing::metadata::LevelFilter::INFO.into())
            .add_directive(base.parse().unwrap())
    };

    if legacy_openvmm_env("OPENVMM_DISABLE_TRACING_RATELIMITS").is_ok_and(|v| !v.is_empty()) {
        tracelimit::disable_rate_limiting(true);
    }

    let is_terminal = std::io::stderr().is_terminal();
    let writer = if is_terminal {
        // Convert LF to CRLF in logs since the output terminal may be in raw mode.
        BoxMakeWriter::new(|| tracing_helpers::formatter::CrlfWriter::new(std::io::stderr()))
    } else {
        BoxMakeWriter::new(std::io::stderr)
    };

    let format = Format::default()
        .with_timer(uptime())
        .with_ansi(is_terminal);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .event_format(format)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .fmt_fields(tracing_helpers::formatter::FieldFormatter)
        .log_internal_errors(true)
        .with_writer(writer);

    let sub = tracing_subscriber::Registry::default()
        .with(fmt_layer)
        .with(filter);

    // Enable an ETW layer on Windows.
    // TODO: include the process name and maybe a VM ID?
    #[cfg(windows)]
    let sub = {
        let mut etw =
            win_etw_tracing::TracelogSubscriber::new(OPENVMM_PROVIDER_GUID, "Microsoft.HvLite")
                .map_err(|e| anyhow!("failed to start ETW provider: {:?}", e))?;

        // Set a keyword for events at "trace" level to distinguish them from "debug" level events,
        // since both are logged at the ETW "Verbose" level.
        etw.set_trace_keyword(OPENVMM_KEYWORD_TRACE_LEVEL);
        sub.with(etw)
    };

    sub.try_init()
        .map_err(|e| anyhow!(e).context("failed to enable tracing"))?;

    Ok(())
}
