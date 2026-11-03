// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM lifecycle integration tests.

use anyhow::Context;
use mesh::CancelContext;
use petri::PetriHaltReason;
use petri::PetriVmBuilder;
use petri::openvmm::OpenVmmPetriBackend;
use std::time::Duration;
use vmm_test_macros::openvmm_test_no_agent;

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
