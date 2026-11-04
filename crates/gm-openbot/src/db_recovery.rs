//! Fail-closed opens and explicitly requested, exclusive corruption recovery.
use gm_core::{
    database::{Database, acquire_database_lease, corrupt_header, integrity},
    error::{GmError, Result},
};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Platform(format!("Database preserved; recovery refused: {e}"))
}

pub fn check_sqlite_integrity(path: &Path) -> Result<()> {
    if !path.try_exists()? {
        return Ok(());
    }
    if corrupt_header(path)? {
        return Err(error("Invalid SQLite header"));
    }
    let conn =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(error)?;
    conn.busy_timeout(std::time::Duration::from_millis(250))
        .map_err(error)?;
    if integrity(&conn).map_err(error)? {
        Ok(())
    } else {
        Err(error("Integrity check detected corruption"))
    }
}

/// Compatibility name: ordinary opens no longer perform automatic recovery.
pub fn open_sqlite_with_recovery(path: &Path) -> Result<Database> {
    Database::open(path)
}

/// Preserve every original file in a unique private directory. This command
/// requires all application clients to close, including the runtime owner.
/// Lock/permission/I/O errors never qualify as corruption.
pub fn recover_corrupt_database(path: &Path) -> Result<PathBuf> {
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let _runtime = crate::runtime::RuntimeLease::acquire(root)?;
    let _exclusive = acquire_database_lease(path, true)?;
    let corrupt = if corrupt_header(path)? {
        true
    } else {
        let conn =
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(error)?;
        conn.busy_timeout(std::time::Duration::from_millis(250))
            .map_err(error)?;
        match integrity(&conn) {
            Ok(ok) => !ok,
            Err(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) =>
            {
                true
            }
            Err(e) => return Err(error(e)),
        }
    };
    if !corrupt {
        return Err(error("Database is healthy"));
    }
    let backup = root.join(format!(
        "corrupt-{}-{}.bak",
        chrono::Utc::now().format("%Y%m%dT%H%M%S"),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&backup)?;
    gm_core::storage::ensure_private_dir(&backup)?;
    let mut originals = vec![];
    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        let source = PathBuf::from(name);
        match std::fs::symlink_metadata(&source) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => continue,
            _ => {
                return Err(error(
                    "Database component is missing, inaccessible or not a regular file",
                ));
            }
        }
        let target = backup.join(
            source
                .file_name()
                .ok_or_else(|| error("Invalid file name"))?,
        );
        let mut input = std::fs::File::open(&source)?;
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut output = options.open(&target)?;
        let mut hash = gm_core::hash::Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?;
        }
        output.sync_all()?;
        drop(output);
        let mut check = std::fs::File::open(&target)?;
        let mut verified = gm_core::hash::Sha256::new();
        loop {
            let n = check.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            verified.update(&buffer[..n]);
        }
        if hash.finalize() != verified.finalize() {
            return Err(error("Backup checksum mismatch"));
        }
        originals.push(source);
    }
    // Keep original files too. Roll back all moves if any component cannot move.
    let original_dir = backup.join("originals");
    gm_core::storage::ensure_private_dir(&original_dir)?;
    let mut moved: Vec<(PathBuf, PathBuf)> = vec![];
    for source in originals {
        let destination = original_dir.join(
            source
                .file_name()
                .ok_or_else(|| error("Invalid file name"))?,
        );
        if let Err(e) = std::fs::rename(&source, &destination) {
            let mut rollback_errors = vec![];
            for (from, to) in moved.iter().rev() {
                if let Err(failure) = std::fs::rename(to, from) {
                    rollback_errors.push(failure.to_string());
                }
            }
            return Err(error(format!(
                "{e}; originals retained at {}; rollback errors: {rollback_errors:?}",
                backup.display()
            )));
        }
        moved.push((source, destination));
    }
    #[cfg(unix)]
    {
        std::fs::File::open(&backup)?.sync_all()?;
        std::fs::File::open(root)?.sync_all()?;
    }
    // A later normal open creates a fresh schema. Nothing is silently reset.
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let p = std::env::temp_dir().join(format!("gm-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn corrupt_database_requires_explicit_recovery_and_preserves_all_files() {
        let dir = root();
        let path = dir.join("test.sqlite");
        std::fs::write(&path, b"NOT_A_VALID_SQLITE_HEADER_GARBAGE_DATA").unwrap();
        std::fs::write(dir.join("test.sqlite-wal"), b"wal-evidence").unwrap();
        std::fs::write(dir.join("test.sqlite-shm"), b"shm-evidence").unwrap();
        assert!(open_sqlite_with_recovery(&path).is_err());
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"NOT_A_VALID_SQLITE_HEADER_GARBAGE_DATA"
        );
        let backup = recover_corrupt_database(&path).unwrap();
        assert_eq!(
            std::fs::read(backup.join("test.sqlite")).unwrap(),
            b"NOT_A_VALID_SQLITE_HEADER_GARBAGE_DATA"
        );
        assert_eq!(
            std::fs::read(backup.join("test.sqlite-wal")).unwrap(),
            b"wal-evidence"
        );
        assert_eq!(
            std::fs::read(backup.join("test.sqlite-shm")).unwrap(),
            b"shm-evidence"
        );
        drop(open_sqlite_with_recovery(&path).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn healthy_or_live_database_cannot_be_reset() {
        let dir = root();
        let path = dir.join("test.sqlite");
        let db = open_sqlite_with_recovery(&path).unwrap();
        db.execute_batch("CREATE TABLE precious(value TEXT); INSERT INTO precious VALUES ('keep')")
            .unwrap();
        assert!(recover_corrupt_database(&path).is_err());
        drop(db);
        assert!(recover_corrupt_database(&path).is_err());
        let db = open_sqlite_with_recovery(&path).unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM precious", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn contention_never_archives_or_reinitializes() {
        let dir = root();
        let path = dir.join("test.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE precious(id INTEGER); BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(recover_corrupt_database(&path).is_err());
        assert!(path.exists());
        assert!(!std::fs::read_dir(&dir).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("corrupt-")
        }));
        db.execute_batch("ROLLBACK").unwrap();
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
