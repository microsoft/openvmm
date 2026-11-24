// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM lifecycle integration tests.

use anyhow::Context;
use mesh::CancelContext;
use openvmm_defs::rpc::SnapshotQuiesceError;
use pal_async::DefaultDriver;
use pal_async::timer::Instant;
use pal_async::timer::PolledTimer;
use petri::PetriHaltReason;
use petri::PetriVm;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::time::Duration;
use vmm_test_macros::openvmm_test_no_agent;

/// Time for a microVM guest to boot and reach its snapshot request.
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
/// Time for a running microVM guest to take its next step.
const GUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The power-off status of the guests of the snapshot controller tests.
const GUEST_STATUS: u8 = 7;
/// The guest's request for a snapshot with fresh scratch, a write of 0 to the
/// snapshot-request port, 0x605. It returns once the snapshot boundary that
/// holds it is released.
const SNAPSHOT_REQUEST: &str =
    "printf '\\x00' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null\n";

fn portb_output(bytes: &[u8]) -> anyhow::Result<String> {
    use std::fmt::Write as _;

    let mut output = String::new();
    for byte in bytes {
        writeln!(
            output,
            "printf '\\x{byte:02x}' | dd of=/dev/port bs=1 seek=233 count=1 conv=notrunc 2>/dev/null"
        )?;
    }
    Ok(output)
}

#[openvmm_test_no_agent(linux_direct_x64)]
async fn phase_1_lifecycle(config: PetriVmBuilder<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(30);
    const BOOT_MARKER: &[u8] = b"OPENVMM-LINUX-DIRECT-TEST-READY\n";
    const RAW_MARKER: &[u8] = b"\0\r\n\x7f\xffLINUX-DIRECT-ECHO\n";
    const SNAPSHOT_MARKER: &[u8] = b"SNAPSHOT-CONTINUED=1\n";

    // portb takes no host input, so the guest takes its steps without
    // commands: it writes raw bytes, requests a snapshot that has no
    // destination, and once the request continues, shuts down with status 37.
    let script = format!(
        "#!/bin/busybox sh\n\
         mount -t devtmpfs devtmpfs /dev 2>/dev/null || true\n\
         {}\
         {}\
         {}\
         printf '\\x00' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null\n\
         {}\
         printf '\\045' | dd of=/dev/port bs=1 seek=1540 count=1 conv=notrunc 2>/dev/null\n\
         while :; do sleep 1; done\n",
        portb_output(BOOT_MARKER)?,
        portb_output(RAW_MARKER)?,
        portb_output(b"SNAPSHOT-REQUESTED\n")?,
        portb_output(SNAPSHOT_MARKER)?,
    );
    let config = config.with_microvm_machine(1);
    let initrd = config.prepare_initrd_with_file("microvm-test", script.as_bytes(), 0o100755)?;
    let mut vm = config
        .with_prebuilt_initrd(initrd)
        .modify_backend(|config| {
            config.with_linux_command_line(|cmdline| {
                assert!(cmdline.contains("rdinit=/microvm-test"));
            })
        })
        .run_without_agent()
        .await?;

    for (marker, failure) in [
        (
            BOOT_MARKER,
            "timed out waiting for microVM Linux direct boot marker",
        ),
        (RAW_MARKER, "microVM raw binary output was not observed"),
        (
            SNAPSHOT_MARKER,
            "microVM snapshot request without a destination did not continue",
        ),
    ] {
        CancelContext::new()
            .with_timeout(TIMEOUT)
            .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(marker))
            .await
            .context(failure)??;
    }

    let halt = CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for microVM status shutdown")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains("code: 37"),
        "expected microVM shutdown status 37, got {}",
        halt.detail
    );

    // The worker still serves the halted VM's management requests, and the
    // microVM refuses host saves whatever the guest's state.
    let save_error = vm
        .backend()
        .save_state()
        .await
        .expect_err("microVM host save unexpectedly succeeded");
    assert!(
        format!("{save_error:#}").contains("save is unavailable for microVM"),
        "unexpected microVM host-save error: {save_error:#}"
    );
    let pulse_error = vm
        .backend()
        .pulse_save_restore()
        .await
        .expect_err("microVM pulse save/restore unexpectedly succeeded");
    assert!(
        format!("{pulse_error:#}")
            .contains("save and restore are unavailable for this machine profile"),
        "unexpected microVM pulse-save error: {pulse_error:#}"
    );

    vm.teardown().await
}

/// Returns the guest's command that powers the microVM off with `status`, a
/// write to the shutdown port, 0x604.
fn power_off(status: u8) -> String {
    format!(
        "printf '\\{status:03o}' | dd of=/dev/port bs=1 seek=1540 count=1 conv=notrunc 2>/dev/null\n"
    )
}

/// Boots a 1-vCPU microVM whose init runs `script` and then idles, with the
/// test as its snapshot controller.
async fn run_snapshot_controller_guest(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    script: &str,
) -> anyhow::Result<PetriVm<OpenVmmPetriBackend>> {
    let script = format!(
        "#!/bin/busybox sh\n\
         mount -t devtmpfs devtmpfs /dev 2>/dev/null || true\n\
         {script}\
         while :; do sleep 1; done\n"
    );
    let config = config.with_microvm_machine(1);
    let initrd = config.prepare_initrd_with_file("microvm-test", script.as_bytes(), 0o100755)?;
    config
        .with_prebuilt_initrd(initrd)
        .modify_backend(|config| config.with_microvm_snapshot_boundary())
        .run_without_agent()
        .await
}

/// Waits for the VM worker to hold the guest's snapshot request at a snapshot
/// boundary.
async fn wait_for_snapshot_boundary(vm: &mut PetriVm<OpenVmmPetriBackend>) -> anyhow::Result<()> {
    CancelContext::new()
        .with_timeout(BOOT_TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_snapshot_boundary())
        .await
        .context("the guest's snapshot request reached no snapshot boundary")??;
    Ok(())
}

/// Waits for the guest to write `marker` to portb, failing with `failure`.
async fn wait_for_guest_output(
    vm: &mut PetriVm<OpenVmmPetriBackend>,
    marker: &[u8],
    failure: &'static str,
) -> anyhow::Result<()> {
    CancelContext::new()
        .with_timeout(GUEST_TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(marker))
        .await
        .context(failure)?
}

/// Waits for the guest to power the microVM off with `status`.
async fn wait_for_power_off(
    vm: &mut PetriVm<OpenVmmPetriBackend>,
    status: u8,
) -> anyhow::Result<()> {
    let halt = CancelContext::new()
        .with_timeout(GUEST_TIMEOUT)
        .until_cancelled(vm.wait_for_halt())
        .await
        .context("timed out waiting for the guest to power off")??;
    assert_eq!(halt.reason, PetriHaltReason::PowerOff);
    assert!(
        halt.detail.contains(&format!("code: {status}")),
        "expected microVM shutdown status {status}, got {}",
        halt.detail
    );
    Ok(())
}

/// Returns the state of the VM worker's partition state unit, which runs the
/// vCPUs: `running`, or `stopped` once the VM quiesced.
async fn partition_unit_state(vm: &PetriVm<OpenVmmPetriBackend>) -> anyhow::Result<String> {
    let node = vm.inspect_vmm("partition/unit_state").await?;
    let state: serde_json::Value = serde_json::from_str(&node.json().to_string())
        .context("failed to parse the partition's unit state")?;
    state
        .as_str()
        .map(str::to_owned)
        .context("the partition's unit state is not a string")
}

/// The test, as the microVM's snapshot controller, takes each of the
/// controller's ways out of a guest-requested snapshot boundary: it releases
/// the first boundary without a snapshot, and quiesces the VM for the second
/// one's snapshot, then rolls the snapshot back. Each time the guest
/// continues after its snapshot request, and an ordinary resume cannot pass
/// the boundary.
#[openvmm_test_no_agent(linux_direct_x64)]
async fn snapshot_boundary_release_and_rollback(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const RELEASED: &[u8] = b"SNAPSHOT-RELEASED\n";
    const ROLLED_BACK: &[u8] = b"SNAPSHOT-ROLLED-BACK\n";

    let mut vm = run_snapshot_controller_guest(
        config,
        &format!(
            "{SNAPSHOT_REQUEST}{}{SNAPSHOT_REQUEST}{}{}",
            portb_output(RELEASED)?,
            portb_output(ROLLED_BACK)?,
            power_off(GUEST_STATUS),
        ),
    )
    .await?;

    wait_for_snapshot_boundary(&mut vm).await?;
    vm.backend()
        .resume()
        .await
        .expect_err("an ordinary resume passed a snapshot boundary");
    vm.backend().release_microvm_snapshot_boundary().await?;
    wait_for_guest_output(
        &mut vm,
        RELEASED,
        "the guest did not continue after its snapshot boundary's release",
    )
    .await?;

    wait_for_snapshot_boundary(&mut vm).await?;
    let snapshot = vm
        .backend()
        .quiesce_microvm_for_snapshot()
        .await?
        .context("the snapshot quiesce failed")?;
    assert!(!snapshot.state_unit_names.is_empty());
    vm.backend()
        .resume()
        .await
        .expect_err("an ordinary resume passed a snapshot boundary");
    vm.backend().resume_microvm_after_failed_snapshot().await?;
    wait_for_guest_output(
        &mut vm,
        ROLLED_BACK,
        "the guest did not continue after its snapshot's rollback",
    )
    .await?;

    wait_for_power_off(&mut vm, GUEST_STATUS).await?;
    vm.teardown().await
}

/// Once the VM worker has quiesced a microVM for the snapshot of its
/// snapshot boundary, only the snapshot's rollback runs the VM again: the
/// boundary's release, which a snapshot controller sends only when it takes
/// no snapshot, is refused, and so is an ordinary resume, so the VM stays
/// stopped until the rollback.
#[openvmm_test_no_agent(linux_direct_x64)]
async fn snapshot_quiesce_resumes_only_on_rollback(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const CONTINUED: &[u8] = b"SNAPSHOT-CONTINUED\n";
    /// How long the quiesced VM is watched for running again.
    const STOPPED_WINDOW: Duration = Duration::from_secs(5);

    let mut vm = run_snapshot_controller_guest(
        config,
        &format!(
            "{SNAPSHOT_REQUEST}{}{}",
            portb_output(CONTINUED)?,
            power_off(GUEST_STATUS),
        ),
    )
    .await?;

    wait_for_snapshot_boundary(&mut vm).await?;
    let snapshot = vm
        .backend()
        .quiesce_microvm_for_snapshot()
        .await?
        .context("the snapshot quiesce failed")?;
    assert!(!snapshot.state_unit_names.is_empty());

    let release = vm.backend().release_microvm_snapshot_boundary().await;
    let resume = vm.backend().resume().await;
    let output = CancelContext::new()
        .with_timeout(STOPPED_WINDOW)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(CONTINUED))
        .await;
    let state = partition_unit_state(&vm).await?;
    assert!(
        release.is_err() && resume.is_err() && output.is_err() && state == "stopped",
        "the quiesced VM ran again without the snapshot's rollback: the boundary's \
         release returned {release:?}, the ordinary resume returned {resume:?}, the \
         guest's continuation within {STOPPED_WINDOW:?} was {output:?}, and the \
         partition is {state}"
    );

    vm.backend().resume_microvm_after_failed_snapshot().await?;
    wait_for_guest_output(
        &mut vm,
        CONTINUED,
        "the guest did not continue after its snapshot's rollback",
    )
    .await?;
    wait_for_power_off(&mut vm, GUEST_STATUS).await?;
    vm.teardown().await
}

/// The VM worker quiesces a microVM for a snapshot only at a snapshot
/// boundary that its guest requested: outside one, the quiesce is rejected
/// and leaves the VM running, since no boundary would then keep an ordinary
/// resume from running the VM again after its state was saved.
#[openvmm_test_no_agent(linux_direct_x64)]
async fn snapshot_quiesce_requires_a_boundary(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const READY: &[u8] = b"SNAPSHOT-GUEST-READY\n";

    let mut vm = run_snapshot_controller_guest(config, &portb_output(READY)?).await?;

    CancelContext::new()
        .with_timeout(BOOT_TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(READY))
        .await
        .context("timed out waiting for the microVM guest to boot")??;
    let quiesce = vm.backend().quiesce_microvm_for_snapshot().await?;
    let state = partition_unit_state(&vm).await?;
    assert!(
        matches!(quiesce, Err(SnapshotQuiesceError::Rejected(_))) && state == "running",
        "a snapshot quiesce outside a snapshot boundary returned {:?}, and left the \
         partition {state}",
        quiesce.map(|snapshot| snapshot.state_unit_names)
    );
    vm.teardown().await
}

/// The VM worker rolls a microVM's snapshot back only at the snapshot
/// boundary that holds the VM: outside one, the rollback is rejected and
/// leaves a paused VM stopped, since it would start the VM without what an
/// ordinary resume does first, which then starts it.
#[openvmm_test_no_agent(linux_direct_x64)]
async fn snapshot_rollback_requires_a_boundary(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    const READY: &[u8] = b"SNAPSHOT-GUEST-READY\n";

    let mut vm = run_snapshot_controller_guest(config, &portb_output(READY)?).await?;

    CancelContext::new()
        .with_timeout(BOOT_TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(READY))
        .await
        .context("timed out waiting for the microVM guest to boot")??;
    vm.backend().pause().await?;
    let rollback = vm.backend().resume_microvm_after_failed_snapshot().await;
    let state = partition_unit_state(&vm).await?;
    assert!(
        rollback.is_err() && state == "stopped",
        "a snapshot rollback outside a snapshot boundary returned {rollback:?}, and left \
         the paused VM's partition {state}"
    );

    vm.backend().resume().await?;
    assert_eq!(partition_unit_state(&vm).await?, "running");
    vm.teardown().await
}

/// The VM worker stops a VM whose guest does not acknowledge its post-restore
/// input gate within the gate's timeout: a guest that starts and then requests
/// a snapshot, its acknowledgement, only after the timeout never reaches a
/// snapshot boundary, since the worker has torn the VM down by then. The
/// worker gates the first start of a cold boot as it gates a restore's, so the
/// test arms the gate of a cold boot.
#[openvmm_test_no_agent(linux_direct_x64)]
async fn restore_gate_stops_a_late_acknowledgement(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
) -> anyhow::Result<()> {
    /// The gate's timeout, within which the guest starts.
    const GATE_TIMEOUT: Duration = Duration::from_secs(5);
    const READY: &[u8] = b"GATE-GUEST-READY\n";

    let mut vm = run_snapshot_controller_guest(
        config.modify_backend(|config| config.with_microvm_restore_gate(GATE_TIMEOUT)),
        &format!(
            "{}sleep {}\n{SNAPSHOT_REQUEST}",
            portb_output(READY)?,
            2 * GATE_TIMEOUT.as_secs(),
        ),
    )
    .await?;

    wait_for_guest_output(
        &mut vm,
        READY,
        "the guest did not start within its gate's timeout",
    )
    .await?;
    let boundary = CancelContext::new()
        .with_timeout(BOOT_TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_snapshot_boundary())
        .await
        .context("the VM worker kept the VM past its gate's timeout")?;
    assert!(
        boundary.is_err(),
        "the guest's acknowledgement after the gate's timeout reached a snapshot boundary"
    );
    vm.teardown().await
}

/// The timeout of a microVM's post-restore input gate bounds only the guest's
/// acknowledgement: the snapshot boundary that holds the acknowledgement holds
/// the VM, with host input still gated, until the VM's snapshot controller
/// releases it, however long after the gate's deadline. The test arms the
/// gate of a cold boot, which the VM worker gates as it gates a restore, and
/// releases the guest's acknowledgement only once the deadline has passed.
#[openvmm_test_no_agent(linux_direct_x64)]
async fn restore_gate_deadline_ends_at_the_acknowledgement(
    config: PetriVmBuilder<OpenVmmPetriBackend>,
    _: (),
    driver: DefaultDriver,
) -> anyhow::Result<()> {
    /// The gate's timeout, within which the guest boots and acknowledges.
    const GATE_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long after the gate's deadline the test releases the
    /// acknowledgement, which leaves the worker time to act on the deadline.
    const RELEASE_DELAY: Duration = Duration::from_secs(2);
    const RELEASED: &[u8] = b"GATE-RELEASED\n";

    let mut vm = run_snapshot_controller_guest(
        config.modify_backend(|config| config.with_microvm_restore_gate(GATE_TIMEOUT)),
        &format!(
            "{SNAPSHOT_REQUEST}{}{}",
            portb_output(RELEASED)?,
            power_off(GUEST_STATUS),
        ),
    )
    .await?;
    // The VM worker armed the gate's deadline when it started the VM, before
    // the launch returned.
    let deadline = Instant::now() + GATE_TIMEOUT;

    wait_for_snapshot_boundary(&mut vm).await?;
    PolledTimer::new(&driver)
        .sleep_until(deadline + RELEASE_DELAY)
        .await;
    vm.backend()
        .release_microvm_snapshot_boundary()
        .await
        .context(
            "the VM worker did not hold the guest's acknowledgement past the gate's deadline",
        )?;
    wait_for_guest_output(
        &mut vm,
        RELEASED,
        "the guest did not continue after its acknowledgement's release",
    )
    .await?;
    wait_for_power_off(&mut vm, GUEST_STATUS).await?;
    vm.teardown().await
}
