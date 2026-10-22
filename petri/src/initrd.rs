// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Boot-mode-independent preparation of Linux initrds.

use crate::PetriInitrd;
use crate::TempOrPersistentPath;
use anyhow::Context;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

/// A file to append to a base initrd, replacing any earlier entry of that name.
pub struct InitrdFile<'a> {
    /// Relative path inside the initrd.
    pub name: &'a str,
    /// Complete file contents.
    pub contents: &'a [u8],
    /// Permissions, without the CPIO file-type bits.
    pub permissions: u32,
}

impl PetriInitrd {
    /// Prepare a temporary initrd from explicit inputs, without inspecting the
    /// host environment or depending on a VM builder or boot mode.
    ///
    /// The base is a gzip-compressed newc CPIO archive and is never modified.
    /// The caller supplies all files (agent, script, certificates, etc.) and
    /// the absolute `rdinit` path. An explicit scratch directory must already
    /// exist; otherwise the system temporary directory is used.
    /// The output is removed when its last owner is dropped.
    pub fn prepare(
        base: &Path,
        files: &[InitrdFile<'_>],
        rdinit: &str,
        scratch_dir: Option<&Path>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            rdinit.starts_with('/') && !rdinit.chars().any(|c| c.is_control() || c.is_whitespace()),
            "rdinit must be an absolute path without whitespace or control characters"
        );
        let mut data = fs_err::read(base).context("reading base initrd")?;
        for file in files {
            anyhow::ensure!(
                !file.name.is_empty()
                    && !file.name.starts_with('/')
                    && !file.name.contains(['\0', '\\'])
                    && file
                        .name
                        .split('/')
                        .all(|part| !matches!(part, "" | "." | "..")),
                "invalid initrd file path: {:?}",
                file.name
            );
            anyhow::ensure!(
                file.permissions & !0o7777 == 0,
                "invalid initrd file permissions"
            );
            data = initrd_cpio::inject_into_initrd(
                &data,
                file.name,
                file.contents,
                0o100000 | file.permissions,
            )
            .with_context(|| format!("injecting {} into initrd", file.name))?;
        }
        let mut builder = tempfile::Builder::new();
        builder.prefix(".petri-initrd-").suffix(".gz");
        let mut output = match scratch_dir {
            Some(dir) => builder.tempfile_in(dir),
            None => builder.tempfile(),
        }
        .context("creating prepared initrd")?;
        output.write_all(&data).context("writing prepared initrd")?;
        Ok(Self {
            path: TempOrPersistentPath::Temp(Arc::new(output.into_temp_path())),
            rdinit_param: rdinit.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use test_with_tracing::test;

    #[test]
    fn prepares_explicit_files_and_owns_temporary_output() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let base = temp.path().join("base.gz");
        let mut trailer = format!(
            "070701{}0000000b00000000TRAILER!!!\0",
            "00000000".repeat(11)
        )
        .into_bytes();
        trailer.resize(trailer.len().div_ceil(4) * 4, 0);
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&trailer)?;
        let original = encoder.finish()?;
        fs_err::write(&base, &original)?;
        let files = [
            InitrdFile {
                name: "pipette",
                contents: b"agent",
                permissions: 0o755,
            },
            InitrdFile {
                name: "init.sh",
                contents: b"exec /pipette",
                permissions: 0o755,
            },
            InitrdFile {
                name: "ca.pem",
                contents: b"explicit CA bundle",
                permissions: 0o644,
            },
        ];
        let first = PetriInitrd::prepare(&base, &files, "/init.sh", Some(temp.path()))?;
        let second = PetriInitrd::prepare(&base, &files, "/init.sh", None)?;
        assert_eq!(first.path.as_ref().parent(), Some(temp.path()));
        assert_eq!(
            second.path.as_ref().parent(),
            Some(std::env::temp_dir().as_path())
        );
        assert_ne!(first.path.as_ref(), second.path.as_ref());
        assert_eq!(first.rdinit_param, "/init.sh");
        assert_eq!(fs_err::read(&base)?, original);
        let mut archive = Vec::new();
        flate2::read::GzDecoder::new(fs_err::File::open(first.path.as_ref())?)
            .read_to_end(&mut archive)?;
        let mut offset = 0;
        for file in files {
            assert_eq!(&archive[offset..offset + 6], b"070701");
            let field = |start, end| {
                u32::from_str_radix(
                    std::str::from_utf8(&archive[offset + start..offset + end]).unwrap(),
                    16,
                )
                .unwrap() as usize
            };
            assert_eq!(field(14, 22), (0o100000 | file.permissions) as usize);
            let size = field(54, 62);
            let name_size = field(94, 102);
            assert_eq!(
                &archive[offset + 110..offset + 110 + name_size - 1],
                file.name.as_bytes()
            );
            let content_offset = offset + (110 + name_size).div_ceil(4) * 4;
            assert_eq!(
                &archive[content_offset..content_offset + size],
                file.contents
            );
            offset = content_offset + size.div_ceil(4) * 4;
        }
        let path = first.path.as_ref().to_path_buf();
        let owner = first.clone();
        drop(first);
        assert!(path.exists());
        drop(owner);
        assert!(!path.exists());
        for name in ["../escape", "/absolute", "bad\0name"] {
            assert!(
                PetriInitrd::prepare(
                    &base,
                    &[InitrdFile {
                        name,
                        contents: b"",
                        permissions: 0o644
                    }],
                    "/init.sh",
                    Some(temp.path()),
                )
                .is_err()
            );
        }
        Ok(())
    }
}
