// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install `cargo-nextest`.

use flowey::node::prelude::*;
use std::io;
use std::io::Read;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_rust::Node>();
        ctx.import::<crate::download_cargo_nextest::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut done = Vec::new();

        for req in requests {
            done.push(req.0);
        }

        let done = done;

        // -- end of req processing -- //

        if done.is_empty() {
            return Ok(());
        }

        let cargo_nextest_bin = ctx.platform().binary("cargo-nextest");

        let nextest_path = ctx.reqv(|v| {
            crate::download_cargo_nextest::Request::Get(target_lexicon::Triple::host(), v)
        });
        let cargo_home = ctx.reqv(crate::install_rust::Request::GetCargoHome);
        let rust_installed = ctx.reqv(crate::install_rust::Request::EnsureInstalled);

        ctx.emit_rust_step("installing cargo-nextest", |ctx| {
            let nextest_path = nextest_path.claim(ctx);
            let cargo_home = cargo_home.claim(ctx);
            rust_installed.claim(ctx);
            done.claim(ctx);

            move |rt| {
                let nextest_path = rt.read(nextest_path);
                let cargo_home = rt.read(cargo_home);

                install_nextest(
                    &nextest_path,
                    &cargo_home.join("bin").join(&cargo_nextest_bin),
                )
                .context("failed to install cargo-nextest")?;

                Ok(())
            }
        });

        Ok(())
    }
}

fn install_nextest(source: &Path, destination: &Path) -> io::Result<()> {
    if files_match(source, destination)? {
        let permissions = fs_err::metadata(source)?.permissions();
        if fs_err::metadata(destination)?.permissions() != permissions {
            fs_err::set_permissions(destination, permissions)?;
        }
    } else {
        fs_err::copy(source, destination)?;
    }
    Ok(())
}

fn files_match(source: &Path, destination: &Path) -> io::Result<bool> {
    let mut source = fs_err::File::open(source)?;
    let mut destination = match fs_err::File::open(destination) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut remaining = source.metadata()?.len();
    if destination.metadata()?.len() != remaining {
        return Ok(false);
    }

    // Do not overwrite an identical executable: it may be running on Windows.
    let mut source_bytes = [0; 16 * 1024];
    let mut destination_bytes = [0; 16 * 1024];
    while remaining != 0 {
        let len = remaining.min(source_bytes.len() as u64) as usize;
        source.read_exact(&mut source_bytes[..len])?;
        destination.read_exact(&mut destination_bytes[..len])?;
        if source_bytes[..len] != destination_bytes[..len] {
            return Ok(false);
        }
        remaining -= len as u64;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn installs_missing_and_changed_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("download");
        let destination = dir.path().join("installed");
        let mut contents = vec![0x5a; 32 * 1024 + 1];
        fs_err::write(&source, &contents).unwrap();
        install_nextest(&source, &destination).unwrap();
        assert_eq!(fs_err::read(&destination).unwrap(), contents);

        contents[32 * 1024] = 0xa5;
        fs_err::write(&source, &contents).unwrap();
        install_nextest(&source, &destination).unwrap();
        assert_eq!(fs_err::read(&destination).unwrap(), contents);

        fs_err::write(&source, b"shorter").unwrap();
        install_nextest(&source, &destination).unwrap();
        assert_eq!(fs_err::read(&destination).unwrap(), b"shorter");
    }

    #[test]
    fn missing_source_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("installed");
        fs_err::write(&destination, b"installed").unwrap();
        assert_eq!(
            install_nextest(&dir.path().join("missing"), &destination)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs_err::read(destination).unwrap(), b"installed");
    }

    #[cfg(windows)]
    #[test]
    fn identical_binary_is_not_overwritten_while_in_use() {
        use fs_err::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("download.exe");
        let destination = dir.path().join("installed.exe");
        fs_err::write(&source, b"nextest").unwrap();
        install_nextest(&source, &destination).unwrap();

        let _in_use = fs_err::OpenOptions::new()
            .read(true)
            .share_mode(1) // FILE_SHARE_READ: deny overwrites as a running image does.
            .open(&destination)
            .unwrap();
        install_nextest(&source, &destination).unwrap();

        fs_err::write(&source, b"updated").unwrap();
        assert!(install_nextest(&source, &destination).is_err());
        assert_eq!(fs_err::read(&destination).unwrap(), b"nextest");
    }

    #[cfg(unix)]
    #[test]
    fn identical_binary_recovers_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("download");
        let destination = dir.path().join("installed");
        fs_err::write(&source, b"nextest").unwrap();
        fs_err::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        install_nextest(&source, &destination).unwrap();
        fs_err::set_permissions(&destination, std::fs::Permissions::from_mode(0o644)).unwrap();
        install_nextest(&source, &destination).unwrap();
        assert_eq!(
            fs_err::metadata(destination).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
