// Copyright (C) Microsoft Corporation. All rights reserved.

use anyhow::anyhow;
use anyhow::Context as _;
use std::io::IsTerminal;
use tracing_subscriber::fmt::format::Format;
use tracing_subscriber::fmt::time::uptime;

/// Enables tracing output to stderr.
pub fn enable_tracing() -> anyhow::Result<()> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    // Enable tracing for underhill_log by default since this is passed through
    // from the guest (but still allow it to be disabled via HVLITE_LOG).
    let base = "underhill_log=trace";
    let filter = if let Ok(filter) = std::env::var("HVLITE_LOG") {
        tracing_subscriber::EnvFilter::try_new(format!("{base},{filter}"))
            .context("invalid HVLITE_LOG")?
    } else {
        tracing_subscriber::EnvFilter::default()
            .add_directive(tracing::metadata::LevelFilter::INFO.into())
            .add_directive(base.parse().unwrap())
    };

    if std::env::var("HVLITE_DISABLE_TRACING_RATELIMITS").map_or(false, |v| !v.is_empty()) {
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
        .fmt_fields(tracing_helpers::formatter::FieldFormatter)
        .log_internal_errors(true)
        .with_writer(writer);

    let sub = tracing_subscriber::Registry::default()
        .with(fmt_layer)
        .with(filter);

    // Enable an ETW layer on Windows.
    // TODO: include the process name and maybe a VM ID?
    #[cfg(windows)]
    let sub = sub.with(
        win_etw_tracing::TracelogSubscriber::new(
            winapi::shared::guiddef::GUID::from(
                "22bc55fe-2116-5adc-12fb-3fadfd7e360c"
                    .parse::<guid::Guid>()
                    .unwrap(),
            ),
            "Microsoft.HvLite",
        )
        .map_err(|e| anyhow!("failed to start ETW provider: {:?}", e))?,
    );

    sub.try_init()
        .map_err(|e| anyhow!(e).context("failed to enable tracing"))?;

    Ok(())
}
