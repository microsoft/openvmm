// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Test entrypoint for CCA emulation tests.

#![forbid(unsafe_code)]

use anyhow::Context as _;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const CCA_TEST_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const CCA_TEST_SUCCESS_MARKER: &str = "PASS";
const CCA_PLANE0_PROMPT: &str = "sh-5.2#";
const CCA_START_TMK_COMMAND: &str = "/root/busybox sh /root/start-tmk.sh";
const CCA_OUTPUT_WINDOW_SIZE: usize = 64 * 1024;
const CCA_TEST_FAILURE_MARKERS: &[&str] = &[
    "test failed",
    "some tests failed",
    "[realm-launch][ERROR]",
    "Kernel panic",
    "panicked at",
];

struct CcaRuntimeArtifacts {
    shrinkwrap_exe: petri::ResolvedArtifact,
    venv_dir: petri::ResolvedArtifact,
    rootfs_file: petri::ResolvedArtifact,
    e2fsck_bin: petri::ResolvedArtifact,
    resize2fs_bin: petri::ResolvedArtifact,
    debugfs_bin: petri::ResolvedArtifact,
    tmk_vmm_bin: petri::ResolvedArtifact,
    simple_tmk_bin: petri::ResolvedArtifact,
    guest_disk: petri::ResolvedArtifact,
    plane0_linux_image: petri::ResolvedArtifact,
    kvmtool_efi: petri::ResolvedArtifact,
    lkvm: petri::ResolvedArtifact,
}

impl CcaRuntimeArtifacts {
    fn validate(&self) -> anyhow::Result<()> {
        for (name, path) in self.paths() {
            if !path.exists() {
                anyhow::bail!("{name} points to missing path {}", path.display());
            }

            tracing::info!(artifact = name, path = %path.display(), "resolved CCA runtime artifact");
        }

        Ok(())
    }

    fn paths(&self) -> [(&'static str, &Path); 12] {
        [
            ("cca::SHRINKWRAP", self.shrinkwrap_exe.get()),
            ("cca::VENV", self.venv_dir.get()),
            ("cca::ROOTFS", self.rootfs_file.get()),
            ("cca::E2FSCK", self.e2fsck_bin.get()),
            ("cca::RESIZE2FS", self.resize2fs_bin.get()),
            ("cca::DEBUGFS", self.debugfs_bin.get()),
            ("tmks::TMK_VMM_LINUX_AARCH64", self.tmk_vmm_bin.get()),
            ("tmks::SIMPLE_TMK_AARCH64", self.simple_tmk_bin.get()),
            ("cca::GUEST_DISK", self.guest_disk.get()),
            ("cca::PLANE0_LINUX_IMAGE", self.plane0_linux_image.get()),
            ("cca::KVMTOOL_EFI", self.kvmtool_efi.get()),
            ("cca::LKVM", self.lkvm.get()),
        ]
    }
}

fn resolve_cca_runtime(resolver: &petri::ArtifactResolver<'_>) -> Option<CcaRuntimeArtifacts> {
    Some(CcaRuntimeArtifacts {
        shrinkwrap_exe: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::SHRINKWRAP)
            .erase(),
        venv_dir: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::VENV)
            .erase(),
        rootfs_file: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::ROOTFS)
            .erase(),
        e2fsck_bin: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::E2FSCK)
            .erase(),
        resize2fs_bin: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::RESIZE2FS)
            .erase(),
        debugfs_bin: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::DEBUGFS)
            .erase(),
        tmk_vmm_bin: resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::TMK_VMM_LINUX_AARCH64)
            .erase(),
        simple_tmk_bin: resolver
            .require(petri_artifacts_vmm_test::artifacts::tmks::SIMPLE_TMK_AARCH64)
            .erase(),
        guest_disk: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::GUEST_DISK)
            .erase(),
        plane0_linux_image: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::PLANE0_LINUX_IMAGE)
            .erase(),
        kvmtool_efi: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::KVMTOOL_EFI)
            .erase(),
        lkvm: resolver
            .require(petri_artifacts_vmm_test::artifacts::cca::LKVM)
            .erase(),
    })
}

fn cca_runtime(
    params: petri::PetriTestParams<'_>,
    artifacts: CcaRuntimeArtifacts,
) -> anyhow::Result<()> {
    artifacts.validate()?;
    let rootfs = prepare_cca_rootfs(&artifacts)?;
    tracing::info!("launching openvmm cca tests...");

    let venv_bin_path = format!(
        "{}:{}",
        artifacts.venv_dir.get().join("bin").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    run_shrinkwrap_cca_test(
        artifacts.shrinkwrap_exe.get(),
        artifacts.venv_dir.get(),
        rootfs.path(),
        &venv_bin_path,
        params.logger.log_file("shrinkwrap_stdout")?,
        params.logger.log_file("shrinkwrap_stderr")?,
    )?;

    tracing::info!("openvmm cca tests finished");

    Ok(())
}

struct PreparedCcaRootfs {
    test_dir: tempfile::TempDir,
    rootfs_path: PathBuf,
}

impl PreparedCcaRootfs {
    fn path(&self) -> &Path {
        debug_assert!(self.rootfs_path.starts_with(self.test_dir.path()));
        &self.rootfs_path
    }
}

fn prepare_cca_rootfs(artifacts: &CcaRuntimeArtifacts) -> anyhow::Result<PreparedCcaRootfs> {
    let test_dir = tempfile::tempdir().context("failed to create CCA runtime test directory")?;
    let rootfs_path = test_dir.path().join("rootfs.ext2");
    std::fs::copy(artifacts.rootfs_file.get(), &rootfs_path).with_context(|| {
        format!(
            "failed to copy CCA rootfs from {} to {}",
            artifacts.rootfs_file.get().display(),
            rootfs_path.display()
        )
    })?;
    tracing::info!(
        source = %artifacts.rootfs_file.get().display(),
        rootfs = %rootfs_path.display(),
        "copied CCA rootfs for test run"
    );

    fsck_rootfs(artifacts.e2fsck_bin.get(), &rootfs_path)?;
    tracing::info!("e2fsck finished");

    resize_rootfs(artifacts.resize2fs_bin.get(), &rootfs_path, "1024M")?;
    tracing::info!("resize rootfs to 1024M finished");

    let cca_files = [
        (artifacts.simple_tmk_bin.get(), "simple_tmk"),
        (artifacts.tmk_vmm_bin.get(), "tmk_vmm"),
        (artifacts.guest_disk.get(), "guest-disk.img"),
        (artifacts.plane0_linux_image.get(), "Image"),
        (artifacts.kvmtool_efi.get(), "KVMTOOL_EFI.fd"),
        (artifacts.lkvm.get(), "lkvm"),
    ];
    inject_files_into_cca_rootfs(artifacts.debugfs_bin.get(), &rootfs_path, &cca_files)?;

    tracing::info!(
        "rootfs.ext2 updated successfully with cca firmwares, paravisor, and tests injected"
    );

    Ok(PreparedCcaRootfs {
        test_dir,
        rootfs_path,
    })
}

fn fsck_rootfs(e2fsck_bin: &Path, rootfs_file: &Path) -> anyhow::Result<()> {
    let status = Command::new(e2fsck_bin)
        .arg("-fp")
        .arg(rootfs_file)
        .status()
        .with_context(|| format!("failed to execute {}", e2fsck_bin.display()))?;

    // e2fsck returns 1 when filesystem errors were found and corrected,
    // which is common after killing the FVP and leaving the rootfs dirty.
    match status.code() {
        Some(0 | 1) => Ok(()),
        Some(code) => anyhow::bail!("e2fsck failed with exit code {code}"),
        None => anyhow::bail!("e2fsck was terminated by signal"),
    }
}

fn resize_rootfs(resize2fs_bin: &Path, rootfs_file: &Path, size: &str) -> anyhow::Result<()> {
    let status = Command::new(resize2fs_bin)
        .arg(rootfs_file)
        .arg(size)
        .status()
        .with_context(|| format!("failed to execute {}", resize2fs_bin.display()))?;

    if !status.success() {
        anyhow::bail!("resize2fs failed with exit status {status}");
    }

    Ok(())
}

fn inject_files_into_cca_rootfs(
    debugfs_bin: &Path,
    rootfs_file: &Path,
    files: &[(&Path, &str)],
) -> anyhow::Result<()> {
    let command_file_dir =
        tempfile::tempdir().context("failed to create debugfs command directory")?;
    let command_file = command_file_dir.path().join("debugfs.commands");
    let mut commands = String::from("mkdir /cca\n");

    for (file, file_name) in files {
        commands.push_str(&format!("write {} /cca/{}\n", file.display(), file_name));
        if matches!(*file_name, "simple_tmk" | "tmk_vmm" | "lkvm") {
            commands.push_str(&format!(
                "set_inode_field /cca/{} mode 0100755\n",
                file_name
            ));
        }
    }

    std::fs::write(&command_file, commands).with_context(|| {
        format!(
            "failed to write debugfs command file {}",
            command_file.display()
        )
    })?;

    let output = Command::new(debugfs_bin)
        .arg("-w")
        .arg("-f")
        .arg(&command_file)
        .arg(rootfs_file)
        .output()
        .with_context(|| format!("failed to execute {}", debugfs_bin.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "debugfs failed to inject CCA payload into {}: status {}; stdout: {}; stderr: {}",
            rootfs_file.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    tracing::info!(
        "rootfs.ext2 updated successfully with cca firmwares, paravisor, and tests injected using debugfs"
    );

    Ok(())
}
fn run_shrinkwrap_cca_test(
    shrinkwrap_exe: &Path,
    venv_dir: &Path,
    rootfs_file: &Path,
    venv_bin_path: &str,
    stdout_log: petri::PetriLogFile,
    stderr_log: petri::PetriLogFile,
) -> anyhow::Result<()> {
    let mut emu = start_cca_emulator(
        shrinkwrap_exe,
        venv_dir,
        rootfs_file,
        venv_bin_path,
        stdout_log,
        stderr_log,
    )?;

    emu.wait_for(CCA_PLANE0_PROMPT)?;
    emu.send_line(CCA_START_TMK_COMMAND)?;
    emu.wait_for(CCA_TEST_SUCCESS_MARKER)?;
    // Need to manually kill FVP processes for the petri test since shrinkwrap doesn't wait
    // for them to exit and they can interfere with subsequent test runs if left running
    stop_fvp_processes_for_rootfs(rootfs_file)?;

    let status = emu
        .stop()
        .context("failed to stop shrinkwrap after CCA test completed")?;
    tracing::info!("CCA test passed; stopped shrinkwrap process with status {status}");

    Ok(())
}

fn stop_fvp_processes_for_rootfs(rootfs_file: &Path) -> anyhow::Result<()> {
    let output = Command::new("pgrep")
        .arg("-f")
        .arg(rootfs_file)
        .output()
        .context("failed to execute pgrep for CCA FVP process")?;

    match output.status.code() {
        Some(0) => {}
        Some(1) => {
            tracing::warn!(rootfs = %rootfs_file.display(), "found no FVP process for CCA test rootfs");
            return Ok(());
        }
        Some(code) => anyhow::bail!("pgrep for CCA FVP process failed with exit code {code}"),
        None => anyhow::bail!("pgrep for CCA FVP process was terminated by signal"),
    }

    let stdout = String::from_utf8(output.stdout).context("pgrep output was not utf-8")?;
    for line in stdout.lines() {
        let pid = line
            .parse::<u32>()
            .with_context(|| format!("failed to parse pgrep pid `{line}`"))?;
        tracing::info!(pid, rootfs = %rootfs_file.display(), "found CCA FVP process for test rootfs");
        terminate_process(pid, "TERM")?;
    }

    Ok(())
}

fn terminate_process(pid: u32, signal: &str) -> anyhow::Result<()> {
    let status = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .status()
        .with_context(|| format!("failed to execute kill -{signal} {pid}"))?;

    match status.code() {
        Some(0) => {
            tracing::info!(pid, signal, "sent signal to CCA FVP process");
            Ok(())
        }
        Some(1) => {
            tracing::warn!(pid, signal, "CCA FVP process was already gone");
            Ok(())
        }
        Some(code) => anyhow::bail!("kill -{signal} {pid} failed with exit code {code}"),
        None => anyhow::bail!("kill -{signal} {pid} was terminated by signal"),
    }
}

fn start_cca_emulator(
    shrinkwrap_exe: &Path,
    venv_dir: &Path,
    rootfs_file: &Path,
    venv_bin_path: &str,
    stdout_log: petri::PetriLogFile,
    stderr_log: petri::PetriLogFile,
) -> anyhow::Result<CcaEmulator> {
    let child = Command::new(shrinkwrap_exe)
        .args(["run", "cca-3world.yaml", "--rtvar"])
        .arg(format!("ROOTFS={}", rootfs_file.display()))
        .env("VIRTUAL_ENV", venv_dir)
        .env("PATH", venv_bin_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| "failed to launch guest using shrinkwrap")?;
    let mut child = ChildGuard::new(child);

    let stdin = child
        .as_mut()
        .stdin
        .take()
        .context("failed to capture shrinkwrap stdin")?;
    let stdout = child
        .as_mut()
        .stdout
        .take()
        .context("failed to capture shrinkwrap stdout")?;
    let stderr = child
        .as_mut()
        .stderr
        .take()
        .context("failed to capture shrinkwrap stderr")?;
    let (output_send, output_recv) = mpsc::channel::<String>();

    spawn_output_reader("shrinkwrap stdout", stdout, stdout_log, output_send.clone());
    spawn_output_reader("shrinkwrap stderr", stderr, stderr_log, output_send.clone());
    drop(output_send);

    Ok(CcaEmulator {
        child,
        stdin,
        output_recv,
        output: String::new(),
        started: Instant::now(),
    })
}

struct ChildGuard {
    child: Option<std::process::Child>,
}

impl ChildGuard {
    fn new(child: std::process::Child) -> Self {
        Self { child: Some(child) }
    }

    fn as_mut(&mut self) -> &mut std::process::Child {
        self.child.as_mut().expect("shrinkwrap child is missing")
    }

    fn kill_and_wait(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        let Some(mut child) = self.child.take() else {
            anyhow::bail!("shrinkwrap child is missing");
        };

        let _ = child.kill();
        child
            .wait()
            .context("failed to wait for shrinkwrap process")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct CcaEmulator {
    child: ChildGuard,
    stdin: std::process::ChildStdin,
    output_recv: mpsc::Receiver<String>,
    output: String,
    started: Instant,
}

impl CcaEmulator {
    fn wait_for(&mut self, marker: &str) -> anyhow::Result<()> {
        tracing::info!(marker, "waiting for CCA emulator output");

        loop {
            if self.output.contains(marker) {
                tracing::info!(marker, "observed CCA emulator output");
                return Ok(());
            }

            if let Some(failure_marker) = CCA_TEST_FAILURE_MARKERS
                .iter()
                .find(|marker| self.output.contains(**marker))
            {
                anyhow::bail!(
                    "CCA test failed after observing failure marker `{failure_marker}` while waiting for `{marker}`"
                );
            }

            if let Some(status) = self.child.as_mut().try_wait()? {
                anyhow::bail!(
                    "shrinkwrap exited before CCA emulator output `{marker}` was observed: {status}"
                );
            }

            let remaining = CCA_TEST_TIMEOUT
                .checked_sub(self.started.elapsed())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                let _ = self.child.kill_and_wait();
                anyhow::bail!("timed out waiting for CCA emulator output `{marker}`");
            }

            match self
                .output_recv
                .recv_timeout(remaining.min(Duration::from_millis(500)))
            {
                Ok(output) => self.append_output(&output),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let status = if let Some(status) = self.child.as_mut().try_wait()? {
                        status
                    } else {
                        self.child.kill_and_wait().with_context(|| {
                            format!(
                                "failed to stop shrinkwrap after output ended while waiting for `{marker}`"
                            )
                        })?
                    };
                    anyhow::bail!(
                        "shrinkwrap output ended before CCA emulator output `{marker}` was observed: {status}"
                    );
                }
            }
        }
    }

    fn append_output(&mut self, output: &str) {
        self.output.push_str(output);

        if self.output.len() <= CCA_OUTPUT_WINDOW_SIZE {
            return;
        }

        let drain_len = self.output.len() - CCA_OUTPUT_WINDOW_SIZE;
        let drain_to = self
            .output
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|idx| *idx >= drain_len)
            .unwrap_or(self.output.len());
        self.output.drain(..drain_to);
    }

    fn send_line(&mut self, line: &str) -> anyhow::Result<()> {
        tracing::info!(line, "sending CCA emulator command");
        writeln!(self.stdin, "{line}").context("failed to write command to shrinkwrap stdin")?;
        self.stdin
            .flush()
            .context("failed to flush command to shrinkwrap stdin")
    }

    fn stop(mut self) -> anyhow::Result<std::process::ExitStatus> {
        self.child.kill_and_wait()
    }
}

enum OutputEvent {
    Byte(u8),
    Eof,
    Error(String),
}

fn spawn_output_reader(
    stream_name: &'static str,
    mut stream: impl Read + Send + 'static,
    log_file: petri::PetriLogFile,
    output_send: mpsc::Sender<String>,
) {
    let (byte_send, byte_recv) = mpsc::channel();

    thread::spawn(move || {
        let mut byte = [0];

        loop {
            let event = match stream.read(&mut byte) {
                Ok(0) => OutputEvent::Eof,
                Ok(_) => OutputEvent::Byte(byte[0]),
                Err(err) => OutputEvent::Error(format!("failed to read {stream_name}: {err}")),
            };

            let done = matches!(event, OutputEvent::Eof | OutputEvent::Error(_));
            if byte_send.send(event).is_err() || done {
                break;
            }
        }
    });

    thread::spawn(move || {
        let mut line = Vec::new();
        let mut logged_len = 0;

        loop {
            match byte_recv.recv_timeout(Duration::from_millis(100)) {
                Ok(OutputEvent::Byte(b'\n')) => {
                    if line.ends_with(b"\r") {
                        line.pop();
                    }
                    let line_was_empty = line.is_empty();
                    write_output_line(&log_file, &output_send, &mut line, &mut logged_len);
                    write_output_newline(&log_file, &output_send, line_was_empty);
                }
                Ok(OutputEvent::Byte(byte)) => {
                    line.push(byte);
                    write_visible_output(&line, &mut logged_len, &output_send);
                }
                Ok(OutputEvent::Eof) => {
                    write_output_line(&log_file, &output_send, &mut line, &mut logged_len);
                    break;
                }
                Ok(OutputEvent::Error(line)) => {
                    log_file.write_entry(&line);
                    let _ = output_send.send(line);
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    write_visible_output(&line, &mut logged_len, &output_send);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    write_output_line(&log_file, &output_send, &mut line, &mut logged_len);
                    break;
                }
            }
        }
    });
}

fn write_output_line(
    log_file: &petri::PetriLogFile,
    output_send: &mpsc::Sender<String>,
    line: &mut Vec<u8>,
    logged_len: &mut usize,
) {
    if line.is_empty() {
        return;
    }

    write_visible_output(line, logged_len, output_send);

    let line_string = String::from_utf8_lossy(line).into_owned();
    log_file.write_entry(&line_string);
    line.clear();
    *logged_len = 0;
}

fn write_output_newline(
    log_file: &petri::PetriLogFile,
    output_send: &mpsc::Sender<String>,
    line_was_empty: bool,
) {
    let _ = std::io::stdout().write_all(b"\n");
    let _ = std::io::stdout().flush();
    let _ = output_send.send("\n".to_owned());

    if line_was_empty {
        log_file.write_entry("");
    }
}

fn write_visible_output(line: &[u8], logged_len: &mut usize, output_send: &mpsc::Sender<String>) {
    if *logged_len >= line.len() {
        return;
    }

    let output = &line[*logged_len..];
    let _ = std::io::stdout().write_all(output);
    let _ = std::io::stdout().flush();
    let _ = output_send.send(String::from_utf8_lossy(output).into_owned());
    *logged_len = line.len();
}

petri::multitest!(vec![
    petri::SimpleTest::new(
        "cca_runtime",
        resolve_cca_runtime,
        cca_runtime,
        None,
        false,
        petri::RemoteAccess::LocalOnly,
    )
    .into()
]);

fn main() {
    petri::test_main(|name, requirements| {
        requirements.resolve(
            petri_artifact_resolver_openvmm_known_paths::OpenvmmKnownPathsTestArtifactResolver::new(
                name,
            ),
        )
    })
}
