// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Native x86 partition-time lifecycle coverage (not OpenHCL).

use anyhow::Context;
use pal_async::DefaultDriver;
use pal_async::timer::PolledTimer;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::time::Duration;
use std::time::Instant;
use vmm_test_macros::vmm_test_with;

/// Guest monotonic time excludes stopped time, and a pending guest OS timer
/// survives both a pause and a save/reset/restore while already paused.
#[vmm_test_with(openvmm, requires(windows_partition_reset), configs(linux_direct_x64))]
async fn partition_time_freeze_lifecycle(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    const TIMER_DELAY: Duration = Duration::from_secs(5);
    const HALF_PAUSE: Duration = Duration::from_secs(5);
    const CLOCK_SLACK: Duration = Duration::from_millis(250);

    let (mut vm, mut agent) = config.run().await?;
    mesh::CancelContext::new()
        .with_timeout(Duration::from_secs(120))
        .until_cancelled(async {
            let mut timer = PolledTimer::new(&driver);
            for restore in [false, true] {
                let host_start = Instant::now();
                let armed = agent.timer_probe(Some(TIMER_DELAY)).await?;
                anyhow::ensure!(armed.fired_ns.is_none(), "probe expired before pausing");
                vm.backend().pause().await?;
                // Reconnect for both paths: no probe state may depend on the
                // transport, which save/reset/restore tears down.
                drop(agent);
                let freeze_start = Instant::now();
                timer.sleep(HALF_PAUSE).await;
                if restore {
                    vm.backend().pulse_save_restore().await?;
                }
                timer.sleep(HALF_PAUSE).await;
                let frozen = freeze_start.elapsed();
                vm.backend().resume().await?;
                agent = vm.backend().wait_for_agent(false).await?;

                let mut previous_ns = armed.now_ns;
                let mut first_sample = true;
                loop {
                    let sample = agent.timer_probe(None).await?;
                    let active_budget = host_start.elapsed().saturating_sub(frozen);
                    if first_sample {
                        anyhow::ensure!(
                            active_budget + CLOCK_SLACK < TIMER_DELAY,
                            "excessive host/RPC delay prevents checking the pending timer \
                             across pause (restore={restore}): active={active_budget:?}"
                        );
                        first_sample = false;
                    }
                    anyhow::ensure!(
                        sample.started_ns == armed.started_ns,
                        "guest probe was replaced or restarted (restore={restore})"
                    );
                    anyhow::ensure!(
                        sample.now_ns >= previous_ns && sample.now_ns >= armed.started_ns,
                        "guest monotonic clock regressed (restore={restore}): {sample:?}"
                    );
                    let guest_elapsed = Duration::from_nanos(sample.now_ns - armed.started_ns);
                    tracing::info!(
                        restore,
                        ?frozen,
                        ?active_budget,
                        ?guest_elapsed,
                        ?sample,
                        "partition time probe"
                    );
                    // Include all RPC/reconnect/scheduling overhead in the
                    // active budget, but exclude the measured stopped interval.
                    // Bound the first sample's budget above so RPC boundary
                    // delays cannot mask a fully leaked ten-second pause.
                    anyhow::ensure!(
                        guest_elapsed <= active_budget + CLOCK_SLACK,
                        "stopped time leaked into guest clock (restore={restore}): \
                         guest={guest_elapsed:?}, active={active_budget:?}, frozen={frozen:?}"
                    );
                    if let Some(fired_ns) = sample.fired_ns {
                        anyhow::ensure!(
                            fired_ns >= armed.started_ns
                                && Duration::from_nanos(fired_ns - armed.started_ns) >= TIMER_DELAY
                                && fired_ns <= sample.now_ns
                                && active_budget + CLOCK_SLACK >= TIMER_DELAY,
                            "guest timer fired early (restore={restore}): {sample:?}"
                        );
                        break;
                    }
                    anyhow::ensure!(
                        active_budget < Duration::from_secs(30),
                        "guest timer did not fire after resume (restore={restore}): {sample:?}"
                    );
                    previous_ns = sample.now_ns;
                    timer.sleep(Duration::from_millis(100)).await;
                }
            }
            agent.power_off().await?;
            vm.wait_for_clean_teardown().await?;
            anyhow::Ok(())
        })
        .await
        .context("partition time lifecycle timed out")??;
    Ok(())
}
