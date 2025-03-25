// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::process::Stdio;

pub(crate) async fn livedump() {
    // If a livedump fails we don't want to panic, just log the error.
    match livedump_core().await {
        Err(e) => tracing::error!(?e, "livedump failed"),
        Ok(()) => tracing::info!("livedump succeeded"),
    }
}

async fn livedump_core() -> anyhow::Result<()> {
    if underhill_confidentiality::confidential_filtering_enabled() {
        tracing::info!("livedump disabled due to CVM");
        return Ok(());
    }

    let (dump_read, dump_write) = pal::unix::pipe::pair()?;

    // Spawn underhill-crash to forward the crash dump to the host.
    // Give it what arguments we can, but as this is a live dump they're not quite as relevant.
    let crash_proc = std::process::Command::new("underhill-crash")
        .stdin(dump_read)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env("UNDERHILL_CRASH_NO_REDIRECT", "1")
        .arg(std::process::id().to_string()) // pid
        .arg(0.to_string()) // tid
        .arg(0.to_string()) // sig
        .arg(std::env::current_exe().unwrap_or_default()) // comm
        .spawn()?;

    // Spawn underhill-dump to create the dump.
    // This needs to be done after underhill-crash, as underhill-dump will pause us.
    let dump_result = std::process::Command::new("underhill-dump")
        .arg(format!("{}", std::process::id()))
        .stdin(Stdio::null())
        .stdout(dump_write)
        .stderr(Stdio::piped())
        .output()?;

    // underhill-dump should finish first, as it's the producer.
    let crash_result = crash_proc.wait_with_output()?;

    // Check for errors. If both failed log both outputs, as one crashing may cause the other to fail.
    if !dump_result.status.success() {
        let dump_output = String::from_utf8_lossy(&dump_result.stderr);
        for line in dump_output.lines() {
            tracing::info!("underhill-dump output: {}", line);
        }
    }

    if !crash_result.status.success() {
        let crash_output = String::from_utf8_lossy(&crash_result.stdout);
        for line in crash_output.lines() {
            tracing::info!("underhill-crash output: {}", line);
        }
        anyhow::bail!("underhill-crash failed: {}", crash_result.status);
    }

    if !dump_result.status.success() {
        anyhow::bail!("underhill-dump failed: {}", dump_result.status);
    }

    Ok(())
}
