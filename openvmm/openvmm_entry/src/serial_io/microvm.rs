// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Local serial endpoints and capability input of the microVM control console.

#[cfg(target_os = "linux")]
use serial_socket::net::OpenSocketSerialConfig;
use std::fs::File;
use std::io;
#[cfg(any(unix, windows))]
use std::io::Read;
use std::path::Path;
#[cfg(target_os = "linux")]
use unix_socket::UnixListener;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;

#[cfg(target_os = "linux")]
pub fn bind_control_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control endpoint has no parent directory",
        )
    })?;
    let parent_metadata = fs_err::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink()
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != pal::unix::effective_user_id()
        || parent_metadata.mode() & 0o7777 != 0o700
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control endpoint parent must be a non-symlink directory owned by OpenVMM with mode 0700",
        ));
    }

    let listener = UnixListener::bind(path)?;
    let bound_metadata = fs_err::symlink_metadata(path)?;
    let prepare_result = (|| {
        fs_err::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let socket_metadata = fs_err::symlink_metadata(path)?;
        if !socket_metadata.file_type().is_socket()
            || socket_metadata.dev() != bound_metadata.dev()
            || socket_metadata.ino() != bound_metadata.ino()
            || socket_metadata.uid() != parent_metadata.uid()
            || socket_metadata.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control endpoint must be an owned Unix socket with mode 0600",
            ));
        }
        Ok(())
    })();
    if let Err(error) = prepare_result {
        drop(listener);
        if let Ok(current) = fs_err::symlink_metadata(path)
            && current.file_type().is_socket()
            && current.dev() == bound_metadata.dev()
            && current.ino() == bound_metadata.ino()
        {
            let _ = fs_err::remove_file(path);
        }
        return Err(error);
    }
    Ok(OpenSocketSerialConfig::from(listener).into_resource())
}

#[cfg(windows)]
pub fn bind_control_serial(path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    use pal::windows::security::LocalSecurityDescriptor;
    use serial_socket::windows::OpenWindowsPipeSerialConfig;

    const NAMED_PIPE_PREFIX: &str = "//./pipe/";

    let normalized = path.to_string_lossy().replace('\\', "/");
    let name = normalized.strip_prefix(NAMED_PIPE_PREFIX).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control endpoint must name a Windows //./pipe/... endpoint",
        )
    })?;
    if name.is_empty() || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control endpoint must contain one nonempty Windows named-pipe name",
        ));
    }

    let user_sid = pal::windows::security::user_sid::current_process_user_sid()?;
    let descriptor = format!("D:P(A;;GA;;;SY)(A;;GA;;;{})", user_sid.to_string_sid())
        .parse::<LocalSecurityDescriptor>()?;
    let pipe = pal::windows::pipe::peer::new_named_pipe_with_security(
        path,
        windows_sys::Win32::Foundation::GENERIC_READ
            | windows_sys::Win32::Foundation::GENERIC_WRITE,
        pal::windows::pipe::Disposition::Create,
        pal::windows::pipe::PipeMode::Byte,
        &descriptor,
    )?;
    Ok(OpenWindowsPipeSerialConfig::from(pipe).into_resource())
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn bind_control_serial(_path: &Path) -> io::Result<Resource<SerialBackendHandle>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "secure control-console local endpoints are only supported on Linux",
    ))
}

/// Consumes a one-way pipe containing exactly one nonzero 32-byte control capability.
#[cfg(any(unix, windows))]
pub fn read_control_capability(mut file: File) -> io::Result<[u8; 32]> {
    const CONTROL_CAPABILITY_LEN: usize = 32;

    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;

        if !file.metadata()?.file_type().is_fifo() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control authentication input is not a one-way pipe",
            ));
        }
        pal::unix::pipe::set_nonblocking(&file, true)?;
    }
    #[cfg(windows)]
    {
        use pal::windows::pipe::PipeExt as _;
        use windows_sys::Win32::System::Pipes::PIPE_NOWAIT;

        if !pal::windows::pipe::peer::is_pipe(&file) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control authentication input is not a one-way pipe",
            ));
        }
        file.set_pipe_mode(PIPE_NOWAIT)?;
    }

    let mut capability = [0u8; CONTROL_CAPABILITY_LEN];
    let mut offset = 0;
    while offset != capability.len() {
        match file.read(&mut capability[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control authentication payload has an invalid length",
                ));
            }
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if control_pipe_would_block(&error) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control authentication writer was not closed",
                ));
            }
            Err(error) if control_pipe_is_closed(&error) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "control authentication payload has an invalid length",
                ));
            }
            Err(error) => return Err(error),
        }
    }

    let mut trailing = [0u8; 1];
    loop {
        match file.read(&mut trailing) {
            Ok(0) => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "control authentication payload has an invalid length",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if control_pipe_would_block(&error) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "control authentication writer was not closed",
                ));
            }
            Err(error) if control_pipe_is_closed(&error) => break,
            Err(error) => return Err(error),
        }
    }
    if capability == [0; CONTROL_CAPABILITY_LEN] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control authentication capability must not be zero",
        ));
    }
    Ok(capability)
}

fn control_pipe_would_block(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_NO_DATA as i32)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn control_pipe_is_closed(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(
            error.raw_os_error().map(|value| value as u32),
            Some(
                windows_sys::Win32::Foundation::ERROR_BROKEN_PIPE
                    | windows_sys::Win32::Foundation::ERROR_PIPE_NOT_CONNECTED
            )
        )
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

/// Reads the prepared authentication pipe without taking ownership of descriptor 0.
#[cfg(target_os = "linux")]
pub fn read_control_capability_from_stdin() -> io::Result<[u8; 32]> {
    use std::os::fd::AsFd;

    let stdin = io::stdin();
    read_control_capability(File::from(stdin.as_fd().try_clone_to_owned()?))
}

/// Reads the prepared authentication pipe without taking ownership of handle 0.
#[cfg(windows)]
pub fn read_control_capability_from_stdin() -> io::Result<[u8; 32]> {
    use std::os::windows::io::AsHandle as _;

    let stdin = io::stdin();
    read_control_capability(File::from(stdin.as_handle().try_clone_to_owned()?))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use test_with_tracing::test;

    fn read_capability_payload(payload: &[u8], keep_writer_open: bool) -> io::Result<[u8; 32]> {
        let (read, mut write) = pal::pipe_pair()?;
        write.write_all(payload)?;
        if !keep_writer_open {
            drop(write);
        }
        read_control_capability(read)
    }

    #[test]
    fn control_capability_requires_exact_closed_pipe_payload() {
        let mut capability = [0x5a; 32];
        capability[0] = 0;
        assert_eq!(
            read_capability_payload(&capability, false).unwrap(),
            capability
        );
        for length in [0, 1, 31] {
            assert_eq!(
                read_capability_payload(&vec![0x5a; length], false)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        for length in [33, 64] {
            assert_eq!(
                read_capability_payload(&vec![0x5a; length], false)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for length in [0, 31, 32] {
            assert_eq!(
                read_capability_payload(&vec![0x5a; length], true)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::TimedOut
            );
        }
        assert_eq!(
            read_capability_payload(&[0; 32], false).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn control_capability_rejects_non_pipe_input() {
        assert_eq!(
            read_control_capability(tempfile::tempfile().unwrap())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn control_capability_stdin_child() {
        let Some(expected) = std::env::var_os("OPENVMM_TEST_CONTROL_STDIN") else {
            return;
        };
        let result = read_control_capability_from_stdin();
        match expected.to_str().unwrap() {
            "valid" => {
                let mut capability = [0x5a; 32];
                capability[0] = 0;
                assert_eq!(result.unwrap(), capability);
                assert_eq!(io::stdin().read(&mut [0]).unwrap(), 0);
            }
            "open-writer" => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut),
            "non-pipe" => assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput),
            _ => panic!("unexpected child-test scenario"),
        }
    }

    #[test]
    fn control_capability_uses_prepared_child_stdin() {
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Duration;
        use std::time::Instant;

        for scenario in ["valid", "open-writer", "non-pipe"] {
            let (read, mut write) = pal::pipe_pair().unwrap();
            let mut capability = [0x5a; 32];
            capability[0] = 0;
            write.write_all(&capability).unwrap();
            let _writer = (scenario == "open-writer").then_some(write);
            let input = if scenario == "non-pipe" {
                tempfile::tempfile().unwrap()
            } else {
                read
            };
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "serial_io::microvm::tests::control_capability_stdin_child",
                    "--nocapture",
                ])
                .env("OPENVMM_TEST_CONTROL_STDIN", scenario)
                .stdin(Stdio::from(input))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    let output = child.wait_with_output().unwrap();
                    panic!("stdin capability scenario {scenario} timed out: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "stdin scenario {scenario}: {output:?}"
            );
        }
    }

    #[test]
    fn control_listener_has_private_permissions_and_exclusive_path() {
        let directory = tempfile::tempdir().unwrap();
        fs_err::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.path().join("control.sock");

        let listener = bind_control_serial(&path).unwrap();
        let metadata = fs_err::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(metadata.uid(), pal::unix::effective_user_id());
        assert!(bind_control_serial(&path).is_err());
        drop(listener);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::io::Write as _;
    use test_with_tracing::test;

    #[test]
    fn control_capability_requires_an_exact_closed_windows_pipe() {
        let (read, mut write) = pal::windows::pipe::pair().unwrap();
        let mut capability = [0x5a; 32];
        capability[0] = 0;
        write.write_all(&capability).unwrap();
        drop(write);
        assert_eq!(read_control_capability(read).unwrap(), capability);

        let (read, mut write) = pal::windows::pipe::pair().unwrap();
        write.write_all(&capability[..31]).unwrap();
        drop(write);
        assert_eq!(
            read_control_capability(read).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
