// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Integration tests for `vmgstool copy-igvmfile` and the
//! load-IGVM-from-VMGS flow.

use petri::PetriGuestStateLifetime;
use petri::PetriVmBuilder;
use petri::PetriVmmBackend;
use petri::ResolvedArtifact;
use petri::run_host_cmd;
use petri_artifacts_common::tags::IsVmfwDll;
use petri_artifacts_common::tags::IsVmgsTool;
use petri_artifacts_vmm_test::artifacts::VMGSTOOL_NATIVE;
use petri_artifacts_vmm_test::artifacts::vmfw_dll::CUSTOM_RESOURCE_CODE;
use petri_artifacts_vmm_test::artifacts::vmfw_dll::LATEST_STANDARD_VMFW_DLL_X64;
use std::path::Path;
use std::process::Command;
use vmm_test_macros::vmm_test;

/// End-to-end test for the `vmgstool copy-igvmfile` flow:
///
/// 1. Create an empty VMGS file with `vmgstool create`.
/// 2. Use `vmgstool copy-igvmfile` to extract the IGVM payload from a
///    `vmfirmwareigvm`-style resource DLL and write it into the VMGS file
///    (file id 8, `GUEST_FIRMWARE`).
/// 3. Start a Hyper-V OpenHCL VM without specifying an IGVM file path.
///    Hyper-V is expected to read the IGVM directly from the VMGS file
///    via [`petri::PetriVmBuilder::with_openhcl_from_vmgs`].
/// 4. Confirm that OpenHCL actually came up by issuing an `inspect` against
///    the live paravisor — if Hyper-V had failed to load the IGVM from the
///    VMGS, the VM wouldn't have started at all.
/// 5. As an additional sanity check, dump file id 8 back out of the VMGS
///    and confirm the bytes match the IGVM resource carved out of the DLL.
///
/// The DLL is built by the in-tree `vmfirmwareigvm_dll` crate, whose
/// `resources.rc` stores the IGVM under resource id `1`. That corresponds
/// to `ResourceCode::Custom` in `vmgstool` (the production resource ids
/// `NONCONFIDENTIAL`, `SNP`, etc. live in non-public DLL builds).
///
/// This test is Hyper-V only because OpenVMM does not currently support
/// loading the IGVM from the VMGS file.
#[vmm_test(
    hyperv_openhcl_uefi_x64(vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE, LATEST_STANDARD_VMFW_DLL_X64],
)]
async fn copy_igvmfile_load_from_vmgs<T: PetriVmmBackend>(
    config: PetriVmBuilder<T>,
    (vmgstool, vmfw_dll): (
        ResolvedArtifact<impl IsVmgsTool>,
        ResolvedArtifact<LATEST_STANDARD_VMFW_DLL_X64>,
    ),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();
    let dll_path = vmfw_dll.get();

    // The resource code that `vmgstool copy-igvmfile` will look up in the
    // DLL must match the code under which the IGVM payload was embedded.
    // The artifact's `RESOURCE_CODE` constant is the source of truth; we
    // assert it matches `CUSTOM_RESOURCE_CODE` (and `ResourceCode::Custom`
    // in `vmgstool`) at compile time so this test fails loudly if the
    // artifact ever starts wrapping a differently-keyed DLL.
    const _: () = assert!(
        <LATEST_STANDARD_VMFW_DLL_X64 as IsVmfwDll>::RESOURCE_CODE == CUSTOM_RESOURCE_CODE,
    );
    let resource_code_arg = "CUSTOM";

    // (1) Create the VMGS file.
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("create").arg("--filepath").arg(&vmgs_path);
    run_host_cmd(cmd).await?;

    // (2) Copy the IGVM out of the DLL and into the VMGS file.
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("copy-igvmfile")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--data-path")
        .arg(dll_path)
        .arg("--resource-code")
        .arg(resource_code_arg);
    run_host_cmd(cmd).await?;

    // (3) Boot a Hyper-V OpenHCL VM that takes its IGVM from the VMGS file
    // we just prepared.
    let (mut vm, agent) = config
        .with_openhcl_from_vmgs()
        .with_guest_state_lifetime(PetriGuestStateLifetime::Disk)
        .with_persistent_vmgs(&vmgs_path)
        .run()
        .await?;

    // (4) Confirm OpenHCL is actually running by querying its diag
    // interface. If we got an IGVM mismatch (or no IGVM at all), VTL2
    // would not be reachable, which would surface here as an error.
    vm.test_inspect_openhcl().await?;
    let build_info = vm
        .inspect_openhcl("build_info", Some(1), None)
        .await?
        .to_string();
    tracing::info!(%build_info, "OpenHCL build info from VMGS-loaded IGVM");

    agent.power_off().await?;
    vm.wait_for_clean_shutdown().await?;

    // Hyper-V copies the VMGS into its own working directory rather than
    // using the path we passed in-place, so re-resolve here for the
    // round-trip check.
    let vmgs_path = vm.get_guest_state_file().await?.unwrap_or(vmgs_path);

    // (5) Dump file id 8 back out of the VMGS and compare to the IGVM
    // resource bytes carved out of the DLL. This catches the (unlikely)
    // failure mode where `copy-igvmfile` wrote a corrupted or truncated
    // payload that still happened to be enough for Hyper-V to load.
    let dumped_igvm = temp_dir.path().join("dumped-igvm.bin");
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("dump")
        .arg("--filepath")
        .arg(&vmgs_path)
        .arg("--data-path")
        .arg(&dumped_igvm)
        .arg("--file-id")
        .arg("GUEST_FIRMWARE");
    run_host_cmd(cmd).await?;

    let dumped_bytes = std::fs::read(&dumped_igvm)?;
    let expected_bytes = extract_vmfw_resource(
        dll_path,
        <LATEST_STANDARD_VMFW_DLL_X64 as IsVmfwDll>::RESOURCE_CODE,
    )?;
    anyhow::ensure!(
        dumped_bytes == expected_bytes,
        "IGVM bytes round-tripped through the VMGS did not match the DLL: \
         dumped {} bytes, expected {} bytes",
        dumped_bytes.len(),
        expected_bytes.len(),
    );

    vm.teardown().await?;

    Ok(())
}

/// Extract the `VMFW` resource with the given numeric id from a
/// `vmfirmwareigvm`-style resource DLL, returning the raw payload bytes.
///
/// This mirrors what `vmgstool copy-igvmfile` does internally and is used
/// above to verify that the round-trip through the VMGS preserved the
/// payload exactly.
fn extract_vmfw_resource(dll_path: &Path, resource_id: u32) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    use std::io::Seek;
    use std::io::SeekFrom;

    let mut file = fs_err::File::open(dll_path)?;
    let descriptor = resource_dll_parser::DllResourceDescriptor::new(b"VMFW", resource_id);
    let (start, len) = resource_dll_parser::try_find_resource_from_dll(&file, &descriptor)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "DLL at {} did not parse as a PE file with a VMFW resource id {}",
                dll_path.display(),
                resource_id,
            )
        })?;

    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}
