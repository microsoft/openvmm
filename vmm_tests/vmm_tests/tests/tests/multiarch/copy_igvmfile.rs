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
use petri_artifacts_vmm_test::artifacts::vmfw_dll::LATEST_CVM_VMFW_DLL_X64;
use std::path::Path;
use std::process::Command;
use vmm_test_macros::vmm_test;
use vmm_test_macros::vmm_test_with;

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
/// loading the IGVM from the VMGS file. It is also x64 + SNP only: Hyper-V
/// only loads the firmware IGVM out of the VMGS for a confidential (CVM)
/// guest, so the test runs as an isolated SNP VM wrapping the `X64Cvm`
/// OpenHCL IGVM. On hosts without SNP the petri requirements framework
/// skips it.
#[vmm_test(
    hyperv_openhcl_uefi_x64[snp](vhd(ubuntu_2504_server_x64))[VMGSTOOL_NATIVE, LATEST_CVM_VMFW_DLL_X64],
)]
async fn copy_igvmfile_load_from_vmgs<T: PetriVmmBackend, D: IsVmfwDll>(
    config: PetriVmBuilder<T>,
    (vmgstool, vmfw_dll): (ResolvedArtifact<impl IsVmgsTool>, ResolvedArtifact<D>),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();
    let dll_path = vmfw_dll.get();

    // The resource code that `vmgstool copy-igvmfile` will look up in the
    // DLL must match the code under which the IGVM payload was embedded.
    // The artifact's `RESOURCE_CODE` constant is the source of truth; we
    // assert it matches `CUSTOM_RESOURCE_CODE` (and `ResourceCode::Custom`
    // in `vmgstool`) so this test fails loudly if the artifact ever starts
    // wrapping a differently-keyed DLL.
    assert_eq!(
        D::RESOURCE_CODE,
        CUSTOM_RESOURCE_CODE,
        "this test only supports DLLs keyed under the CUSTOM resource code",
    );
    let resource_code_arg = "CUSTOM";

    // (1) Create the VMGS file, sized to hold the IGVM payload.
    run_host_cmd(create_vmgs_cmd(vmgstool_path, &vmgs_path)).await?;

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
    let expected_bytes = extract_vmfw_resource(dll_path, D::RESOURCE_CODE)?;
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

/// Negative test: `vmgstool copy-igvmfile` overwrite semantics.
///
/// Writing the IGVM into file id 8 (`GUEST_FIRMWARE`) a second time must
/// fail unless `--allow-overwrite` is passed, because the slot already
/// holds a nonzero-length payload. With the flag the second write
/// succeeds. This exercises the `allow_overwrite` plumbing in
/// `vmgstool`'s `copy-igvmfile` path without booting a VM, so it uses a
/// guest-less (`none`) config.
#[vmm_test_with(noagent(
    hyperv_openhcl_uefi_x64(none)[VMGSTOOL_NATIVE, LATEST_CVM_VMFW_DLL_X64],
))]
async fn copy_igvmfile_overwrite<T: PetriVmmBackend, D: IsVmfwDll>(
    _config: PetriVmBuilder<T>,
    (vmgstool, vmfw_dll): (ResolvedArtifact<impl IsVmgsTool>, ResolvedArtifact<D>),
) -> Result<(), anyhow::Error> {
    assert_eq!(
        D::RESOURCE_CODE,
        CUSTOM_RESOURCE_CODE,
        "this test only supports DLLs keyed under the CUSTOM resource code",
    );

    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();
    let dll_path = vmfw_dll.get();

    // Create the VMGS file, sized to hold the IGVM payload.
    run_host_cmd(create_vmgs_cmd(vmgstool_path, &vmgs_path)).await?;

    // First copy populates file id 8.
    run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        dll_path,
        "CUSTOM",
        false,
    ))
    .await?;

    // Second copy WITHOUT `--allow-overwrite` must fail: the slot is
    // already populated with a nonzero-length payload.
    let res = run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        dll_path,
        "CUSTOM",
        false,
    ))
    .await;
    anyhow::ensure!(
        res.is_err(),
        "copy-igvmfile over an existing file id 8 unexpectedly succeeded without --allow-overwrite",
    );

    // Second copy WITH `--allow-overwrite` must succeed.
    run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        dll_path,
        "CUSTOM",
        true,
    ))
    .await?;

    Ok(())
}

/// Negative test: `vmgstool copy-igvmfile` rejects a non-PE data file.
///
/// Feeding `copy-igvmfile` a file that is not a valid PE/DLL must produce
/// a clean error (`Error::IgvmFile`) and a nonzero exit code rather than
/// panicking or writing garbage into the VMGS. Only the `vmgstool` binary
/// is needed; the bogus input is synthesised on the fly, so no resource
/// DLL artifact is required.
#[vmm_test_with(noagent(
    hyperv_openhcl_uefi_x64(none)[VMGSTOOL_NATIVE],
))]
async fn copy_igvmfile_corrupt_dll<T: PetriVmmBackend>(
    _config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();

    // Create the VMGS file, sized to hold the IGVM payload.
    run_host_cmd(create_vmgs_cmd(vmgstool_path, &vmgs_path)).await?;

    // Write a junk "DLL" that is clearly not a PE image.
    let bogus_dll = temp_dir.path().join("not-a-dll.bin");
    std::fs::write(&bogus_dll, b"this is definitely not a PE file")?;

    // copy-igvmfile must fail cleanly on the non-PE input.
    let res = run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        &bogus_dll,
        "CUSTOM",
        false,
    ))
    .await;
    anyhow::ensure!(
        res.is_err(),
        "copy-igvmfile unexpectedly succeeded on a non-PE data file",
    );

    Ok(())
}

/// Negative test: `vmgstool copy-igvmfile` fails when the `--data-path`
/// file does not exist.
///
/// This exercises a different failure branch than
/// [`copy_igvmfile_corrupt_dll`]: a missing file fails at `File::open`
/// (`Error::DataFile`) before any PE parsing happens, whereas a non-PE
/// file fails later in the resource lookup (`Error::IgvmFile`). Both must
/// produce a clean error and a nonzero exit code rather than panicking.
/// Only the `vmgstool` binary is needed; the path is intentionally never
/// created, so no resource DLL artifact is required.
#[vmm_test_with(noagent(
    hyperv_openhcl_uefi_x64(none)[VMGSTOOL_NATIVE],
))]
async fn copy_igvmfile_missing_data_path<T: PetriVmmBackend>(
    _config: PetriVmBuilder<T>,
    (vmgstool,): (ResolvedArtifact<impl IsVmgsTool>,),
) -> Result<(), anyhow::Error> {
    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();

    // Create the VMGS file, sized to hold the IGVM payload.
    run_host_cmd(create_vmgs_cmd(vmgstool_path, &vmgs_path)).await?;

    // Point `--data-path` at a file that was never created.
    let missing_dll = temp_dir.path().join("does-not-exist.dll");
    anyhow::ensure!(
        !missing_dll.exists(),
        "test bug: the supposedly-missing data file actually exists",
    );

    // copy-igvmfile must fail cleanly when the data file is absent.
    let res = run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        &missing_dll,
        "CUSTOM",
        false,
    ))
    .await;
    anyhow::ensure!(
        res.is_err(),
        "copy-igvmfile unexpectedly succeeded on a nonexistent data file",
    );

    Ok(())
}

/// Negative test: `vmgstool copy-igvmfile` fails when the requested
/// resource code is absent from the DLL, and leaves the VMGS untouched.
///
/// The in-tree resource DLL only carries an IGVM under the `CUSTOM`
/// resource code. Requesting a production code such as `SNP` must fail
/// (the resource lookup finds nothing). The resource is read before
/// anything is written, so file id 8 must remain empty — which we verify
/// by then writing it with a `CUSTOM` copy that does *not* pass
/// `--allow-overwrite`.
#[vmm_test_with(noagent(
    hyperv_openhcl_uefi_x64(none)[VMGSTOOL_NATIVE, LATEST_CVM_VMFW_DLL_X64],
))]
async fn copy_igvmfile_missing_resource<T: PetriVmmBackend, D: IsVmfwDll>(
    _config: PetriVmBuilder<T>,
    (vmgstool, vmfw_dll): (ResolvedArtifact<impl IsVmgsTool>, ResolvedArtifact<D>),
) -> Result<(), anyhow::Error> {
    assert_eq!(
        D::RESOURCE_CODE,
        CUSTOM_RESOURCE_CODE,
        "this test relies on the DLL only carrying the CUSTOM resource code",
    );

    let temp_dir = tempfile::tempdir()?;
    let vmgs_path = temp_dir.path().join("test.vmgs");
    let vmgstool_path = vmgstool.get();
    let dll_path = vmfw_dll.get();

    // Create the VMGS file, sized to hold the IGVM payload.
    run_host_cmd(create_vmgs_cmd(vmgstool_path, &vmgs_path)).await?;

    // Request a resource code the DLL does not contain.
    let res = run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        dll_path,
        "SNP",
        false,
    ))
    .await;
    anyhow::ensure!(
        res.is_err(),
        "copy-igvmfile unexpectedly succeeded for a resource code absent from the DLL",
    );

    // File id 8 must still be empty after the failed attempt: a `CUSTOM`
    // copy WITHOUT `--allow-overwrite` should therefore succeed, proving
    // nothing was written above.
    run_host_cmd(copy_igvmfile_cmd(
        vmgstool_path,
        &vmgs_path,
        dll_path,
        "CUSTOM",
        false,
    ))
    .await?;

    Ok(())
}

/// VMGS capacity used by these tests. The default vmgstool capacity
/// (~4 MiB) cannot hold a real OpenHCL IGVM (tens of MiB). vmgstool caps an
/// IGVM at 256 MiB (`MAX_IGVM_SIZE`), so 384 MiB leaves headroom above that
/// ceiling for VMGS metadata and the other file slots.
const VMGS_FILE_SIZE: u64 = 384 * 1024 * 1024;

/// Build a `vmgstool create` command that sizes the VMGS large enough to
/// hold an IGVM payload (see [`VMGS_FILE_SIZE`]).
fn create_vmgs_cmd(vmgstool_path: &Path, vmgs_path: &Path) -> Command {
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("create")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--file-size")
        .arg(VMGS_FILE_SIZE.to_string());
    cmd
}

/// Build a `vmgstool copy-igvmfile` command for the given VMGS file,
/// resource DLL, and resource code, optionally passing `--allow-overwrite`.
fn copy_igvmfile_cmd(
    vmgstool_path: &Path,
    vmgs_path: &Path,
    dll_path: &Path,
    resource_code: &str,
    allow_overwrite: bool,
) -> Command {
    let mut cmd = Command::new(vmgstool_path);
    cmd.arg("copy-igvmfile")
        .arg("--filepath")
        .arg(vmgs_path)
        .arg("--data-path")
        .arg(dll_path)
        .arg("--resource-code")
        .arg(resource_code);
    if allow_overwrite {
        cmd.arg("--allow-overwrite");
    }
    cmd
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
