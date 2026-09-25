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
    const COMMAND_PING: u8 = 1;
    const COMMAND_ECHO: u8 = 2;
    const COMMAND_SNAPSHOT: u8 = 3;
    const COMMAND_STATE: u8 = 5;
    const COMMAND_SHUTDOWN: u8 = 6;

    let script = format!(
        "#!/bin/busybox sh\n\
         mount -t devtmpfs devtmpfs /dev 2>/dev/null || true\n\
         read_port() {{\n\
             dd if=/dev/port bs=1 skip=\"$1\" count=1 2>/dev/null | od -An -tu1 | tr -d ' '\n\
         }}\n\
         next_byte() {{\n\
             while :; do\n\
                 status=$(read_port 234)\n\
                 [ -n \"$status\" ] || status=0\n\
                 if [ $((status & 1)) -ne 0 ]; then\n\
                     read_port 233\n\
                     return\n\
                 fi\n\
             done\n\
         }}\n\
         generation=0\n\
         {}\
         while :; do\n\
             command=$(next_byte)\n\
             case \"$command\" in\n\
                 {COMMAND_PING})\n\
                     {}\
                     ;;\n\
                 {COMMAND_ECHO})\n\
                     {}\
                     ;;\n\
                 {COMMAND_SNAPSHOT})\n\
                     {}\
                     printf '\\x00' | dd of=/dev/port bs=1 seek=1541 count=1 conv=notrunc 2>/dev/null\n\
                     generation=$((generation + 1))\n\
                     {}\
                     ;;\n\
                 {COMMAND_STATE})\n\
                     if [ \"$generation\" = 1 ]; then\n\
                         {}\
                     else\n\
                         {}\
                     fi\n\
                     ;;\n\
                 {COMMAND_SHUTDOWN})\n\
                     status=$(next_byte)\n\
                     octal=$(printf '%03o' \"$status\")\n\
                     printf \"\\\\$octal\" | dd of=/dev/port bs=1 seek=1540 count=1 conv=notrunc 2>/dev/null\n\
                     while :; do sleep 1; done\n\
                     ;;\n\
                 *)\n\
                     {}\
                     ;;\n\
             esac\n\
         done\n",
        portb_output(BOOT_MARKER)?,
        portb_output(b"PONG\n")?,
        portb_output(RAW_MARKER)?,
        portb_output(b"SNAPSHOT-REQUESTED\n")?,
        portb_output(b"SNAPSHOT-CONTINUED=1\n")?,
        portb_output(b"STATE=1\n")?,
        portb_output(b"STATE-INVALID\n")?,
        portb_output(b"UNKNOWN-COMMAND\n")?,
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

    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(BOOT_MARKER))
        .await
        .context("timed out waiting for microVM Linux direct boot marker")??;

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

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_PING])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("PONG"))
        .await
        .context("microVM Linux direct guest did not answer ping")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_ECHO])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_bytes(RAW_MARKER))
        .await
        .context("microVM raw binary echo was not observed")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_SNAPSHOT])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(
            vm.backend()
                .wait_for_microvm_portb_output("SNAPSHOT-CONTINUED=1"),
        )
        .await
        .context("microVM snapshot request without a destination did not continue")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_STATE])
        .await?;
    CancelContext::new()
        .with_timeout(TIMEOUT)
        .until_cancelled(vm.backend().wait_for_microvm_portb_output("STATE=1"))
        .await
        .context("microVM guest state did not persist after the snapshot request")??;

    vm.backend()
        .write_microvm_portb_input(&[COMMAND_SHUTDOWN, 37])
        .await?;

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

    vm.teardown().await
}
