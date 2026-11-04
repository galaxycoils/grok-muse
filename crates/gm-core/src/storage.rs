//! Durable replacement for small private configuration files.
use crate::error::Result;
use std::{fs::OpenOptions, io::Write, path::Path};
pub fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("File has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".gm-write-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Preserve small serialized stores before their format changes. The original
/// is untouched until a verified, fsynced, exclusively created copy exists.
pub fn preserve_private_file(path: &Path, bytes: &[u8]) -> Result<std::path::PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("File has no parent"))?;
    let backup = parent.join(format!(
        "{}-{}.bak",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&backup)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    if std::fs::read(&backup)? != bytes {
        return Err(std::io::Error::other("Backup verification failed; original preserved").into());
    }
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(backup)
}
