//! Utilities used by flowey_core and made avaiable for higher-level crates.

use std::path::Path;

/// Copies the contents of `src` into the directory `dst`.
pub fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs_err::create_dir_all(&dst)?;
    for entry in fs_err::read_dir(src.as_ref())? {
        let entry = entry?;
        let dst = dst.as_ref().join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(entry.path(), dst)?;
        } else {
            fs_err::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}
