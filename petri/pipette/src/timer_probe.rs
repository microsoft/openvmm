// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A guest timer whose lifetime is independent of the host connection.

use anyhow::Context;
use pal_async::DefaultDriver;
use pal_async::task::Spawn;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use parking_lot::Mutex;
use pipette_protocol::TimerProbe;
use std::sync::Arc;
use std::time::Duration;

struct Probe {
    started_ns: u64,
    fired_ns: Arc<Mutex<Option<u64>>>,
}

static PROBE: Mutex<Option<Probe>> = Mutex::new(None);

pub async fn probe(
    driver: &DefaultDriver,
    duration: Option<Duration>,
) -> anyhow::Result<TimerProbe> {
    if let Some(duration) = duration {
        anyhow::ensure!(
            (Duration::from_secs(1)..=Duration::from_secs(60)).contains(&duration),
            "timer duration must be between one and sixty seconds"
        );
        let started = Instant::now();
        let fired_ns = Arc::new(Mutex::new(None));
        {
            let mut probe = PROBE.lock();
            anyhow::ensure!(
                probe.as_ref().is_none_or(|p| p.fired_ns.lock().is_some()),
                "a timer probe is already pending"
            );
            *probe = Some(Probe {
                started_ns: started.as_nanos(),
                fired_ns: fired_ns.clone(),
            });
        }

        let (ready_send, ready_recv) = mesh::oneshot();
        let mut timer = PolledTimer::new(driver);
        driver
            .spawn("timer-probe", async move {
                let mut ready_send = Some(ready_send);
                let fired = std::future::poll_fn(|cx| {
                    let result = timer.poll_until(cx, started + duration);
                    // Acknowledge only after registering the timer with the
                    // guest OS, so the host can pause with a timer pending.
                    if let Some(send) = ready_send.take() {
                        send.send(());
                    }
                    result
                })
                .await;
                *fired_ns.lock() = Some(fired.as_nanos());
            })
            .detach();
        ready_recv.await.context("timer task failed to start")?;
    }

    let probe = PROBE.lock();
    let probe = probe.as_ref().context("no timer probe has been started")?;
    let fired_ns = *probe.fired_ns.lock();
    Ok(TimerProbe {
        started_ns: probe.started_ns,
        now_ns: Instant::now().as_nanos(),
        fired_ns,
    })
}
