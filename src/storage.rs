//! Filesystem capacity checks used before bounded downloads.

use std::path::Path;

use anyhow::{Context, Result, bail};

#[cfg(unix)]
pub(crate) fn ensure_free_space(path: &Path, minimum_bytes: u64) -> Result<()> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};

    let encoded = CString::new(path.as_os_str().as_bytes()).context("caminho contem byte nulo")?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(encoded.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("falha ao consultar espaco em {}", path.display()));
    }
    let stats = unsafe { stats.assume_init() };
    let available = stats.f_bavail.saturating_mul(stats.f_frsize);
    if available < minimum_bytes {
        bail!(
            "espaco insuficiente em {}: {:.1} MiB livres, {:.1} MiB necessarios",
            path.display(),
            available as f64 / 1024.0 / 1024.0,
            minimum_bytes as f64 / 1024.0 / 1024.0
        );
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn ensure_free_space(_path: &Path, _minimum_bytes: u64) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_directory_has_at_least_one_byte() {
        ensure_free_space(Path::new("."), 1).unwrap();
    }
}
