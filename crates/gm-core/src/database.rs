//! Coordinated SQLite ownership and transactional, backed-up schema changes.
use crate::{
    error::{GmError, Result},
    storage::ensure_private_dir,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use std::{
    fs::{File, OpenOptions},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

fn error(e: impl std::fmt::Display) -> GmError {
    GmError::Platform(format!(
        "SQLite operation failed; original data preserved: {e}"
    ))
}

pub fn acquire_database_lease(path: &Path, exclusive: bool) -> Result<File> {
    lease(path, exclusive, false)
}

fn lease(path: &Path, exclusive: bool, wait: bool) -> Result<File> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lease");
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(PathBuf::from(name))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let result = if exclusive {
            file.try_lock()
        } else {
            file.try_lock_shared()
        };
        match result {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock)
                if wait && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(failure) => return Err(error(failure)),
        }
    }
    Ok(file)
}

pub fn integrity(connection: &Connection) -> rusqlite::Result<bool> {
    let result: String = connection.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    // SQLite FTS hooks can return an operational read-only failure as a row.
    // Never classify that as corruption or authorize recovery from it.
    if result.starts_with("unable to validate the inverted index") {
        let code = if result.contains("readonly") {
            rusqlite::ffi::SQLITE_READONLY
        } else if result.contains("locked") || result.contains("busy") {
            rusqlite::ffi::SQLITE_BUSY
        } else {
            rusqlite::ffi::SQLITE_ERROR
        };
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some(result),
        ));
    }
    Ok(result == "ok")
}

fn writable_integrity(connection: &Connection) -> rusqlite::Result<bool> {
    // FTS checks internally issue a special INSERT. Reserve the writer before
    // taking a read snapshot to avoid SQLITE_BUSY_SNAPSHOT during concurrent opens.
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = integrity(connection);
    connection.execute_batch("ROLLBACK")?;
    result
}

/// Reject invalid headers before SQLite can interpret or clean up sidecars.
pub fn corrupt_header(path: &Path) -> Result<bool> {
    use std::io::Read;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(error("Database path must be a regular file, not a symlink"));
    }
    if metadata.len() == 0 {
        return Ok(false);
    }
    let mut header = [0u8; 16];
    match File::open(path)?.read_exact(&mut header) {
        Ok(()) => Ok(&header != b"SQLite format 3\0"),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// The shared lease lives until SQLite has closed. Recovery requires its
/// exclusive counterpart; merely opening a client never resets data.
#[derive(Debug)]
pub struct Database {
    connection: Connection,
    path: Option<PathBuf>,
    _lease: Option<File>,
}
impl Deref for Database {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.connection
    }
}
impl DerefMut for Database {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }
}
impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !parent.try_exists()?
        {
            ensure_private_dir(parent)?;
        }
        let lease = acquire_database_lease(path, false)?;
        if corrupt_header(path)? {
            return Err(error(
                "Invalid database header. Stop all clients and use explicit recovery.",
            ));
        }
        let connection = Connection::open(path).map_err(error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(error)?;
        if !writable_integrity(&connection).map_err(error)? {
            let detail: String = connection
                .query_row("PRAGMA quick_check(1)", [], |r| r.get(0))
                .map_err(error)?;
            return Err(error(format!(
                "Integrity check failed: {detail}. Stop all clients and inspect before explicit recovery."
            )));
        }
        Ok(Self {
            connection,
            path: Some(path.into()),
            _lease: Some(lease),
        })
    }
    pub fn in_memory() -> Result<Self> {
        Ok(Self {
            connection: Connection::open_in_memory().map_err(error)?,
            path: None,
            _lease: None,
        })
    }

    /// A migration has one commit point. Failures roll back schema + version;
    /// backup creation/verification must succeed before any schema mutation.
    pub fn migrate(
        &mut self,
        component: &str,
        version: u32,
        change: impl FnOnce(&Transaction<'_>) -> Result<()>,
    ) -> Result<()> {
        let _migration_lease = if let Some(path) = &self.path {
            let mut name = path.as_os_str().to_os_string();
            name.push(".migration");
            Some(lease(Path::new(&name), true, true)?)
        } else {
            None
        };
        let has_versions: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='gm_schema_versions')",
            [], |r| r.get(0)).map_err(error)?;
        let current: u32 = if has_versions {
            self.connection
                .query_row(
                    "SELECT version FROM gm_schema_versions WHERE component=?1",
                    [component],
                    |r| r.get(0),
                )
                .optional()
                .map_err(error)?
                .unwrap_or(0)
        } else {
            0
        };
        if current > version {
            return Err(GmError::Config(format!(
                "{component} schema {current} is newer than supported {version}; use a newer application. Original preserved."
            )));
        }
        if current == version {
            return Ok(());
        }
        let has_data: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%')",
            [], |r| r.get(0)).map_err(error)?;
        if has_data && let Some(path) = &self.path {
            let root = path
                .parent()
                .unwrap_or(Path::new("."))
                .join("migration-backups");
            if root.try_exists()? && root.symlink_metadata()?.file_type().is_symlink() {
                return Err(error("Migration backup directory must not be a symlink"));
            }
            ensure_private_dir(&root)?;
            let backup = root.join(format!(
                "{}-{}-{}.sqlite",
                chrono::Utc::now().format("%Y%m%dT%H%M%S%.9f"),
                component,
                uuid::Uuid::new_v4()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&backup)?;
            self.connection
                .execute(
                    "VACUUM INTO ?1",
                    [backup
                        .to_str()
                        .ok_or_else(|| error("Backup path is not UTF-8"))?],
                )
                .map_err(error)?;
            file.sync_all()?;
            let check =
                Connection::open_with_flags(&backup, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
                    .map_err(error)?;
            if !writable_integrity(&check).map_err(error)? {
                return Err(error("Migration backup failed verification"));
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        transaction.execute_batch("CREATE TABLE IF NOT EXISTS gm_schema_versions(component TEXT PRIMARY KEY, version INTEGER NOT NULL)").map_err(error)?;
        change(&transaction)?;
        transaction.execute("INSERT INTO gm_schema_versions(component,version) VALUES (?1,?2) ON CONFLICT(component) DO UPDATE SET version=excluded.version",
            rusqlite::params![component, version]).map_err(error)?;
        transaction.commit().map_err(error)
    }
}

/// Explicit retention applies only to verified migration snapshots. Corruption
/// evidence and current application stores are never deleted by this operation.
pub fn prune_migration_backups(storage: &Path, keep: usize) -> Result<usize> {
    if !(1..=10000).contains(&keep) {
        return Err(GmError::Config("Keep must be between 1 and 10000".into()));
    }
    let directory = storage.join("migration-backups");
    if !directory.try_exists()? {
        return Ok(0);
    }
    if directory.symlink_metadata()?.file_type().is_symlink() {
        return Err(error("Backup directory must not be a symlink"));
    }
    let mut snapshots = vec![];
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file()
            && name.ends_with(".sqlite")
            && name.len() > 50
            && name.as_bytes().first().is_some_and(u8::is_ascii_digit)
        {
            let connection = Connection::open_with_flags(
                entry.path(),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(error)?;
            if !integrity(&connection).map_err(error)? {
                return Err(error("A migration snapshot is corrupt; retention refused"));
            }
            snapshots.push((name, entry.path()));
        }
    }
    snapshots.sort_by(|a, b| b.0.cmp(&a.0));
    let mut removed = 0;
    for (_, path) in snapshots.into_iter().skip(keep) {
        std::fs::remove_file(path)?;
        removed += 1;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrations_roll_back_and_reject_future_versions() {
        let mut db = Database::in_memory().unwrap();
        assert!(
            db.migrate("test", 1, |tx| {
                tx.execute_batch("CREATE TABLE payload(id INTEGER)")
                    .map_err(error)?;
                Err(error("interrupted"))
            })
            .is_err()
        );
        let tables: i64 = db
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='payload'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        db.migrate("test", 2, |_| Ok(())).unwrap();
        assert!(db.migrate("test", 1, |_| panic!("must not run")).is_err());
    }
    #[test]
    fn migration_backup_contains_original_rows_and_leases_exclude_recovery() {
        let root = std::env::temp_dir().join(format!("gm-migrate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("data.sqlite");
        let mut db = Database::open(&path).unwrap();
        db.execute_batch("CREATE TABLE payload(id INTEGER); INSERT INTO payload VALUES(42)")
            .unwrap();
        assert!(acquire_database_lease(&path, true).is_err());
        db.migrate("test", 1, |tx| {
            tx.execute_batch("ALTER TABLE payload ADD COLUMN note TEXT")
                .map_err(error)
        })
        .unwrap();
        let backup = std::fs::read_dir(root.join("migration-backups"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let copy = Connection::open(backup).unwrap();
        assert_eq!(
            copy.query_row("SELECT id FROM payload", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            42
        );
        drop(copy);
        drop(db);
        assert!(acquire_database_lease(&path, true).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    #[test]
    fn backup_failure_prevents_schema_changes_and_preserves_data() {
        let root = std::env::temp_dir().join(format!("gm-backup-fail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("db.sqlite");
        let mut db = Database::open(&path).unwrap();
        db.execute_batch("CREATE TABLE payload(id INTEGER); INSERT INTO payload VALUES(7)")
            .unwrap();
        std::fs::write(root.join("migration-backups"), b"not a directory").unwrap();
        assert!(
            db.migrate("test", 1, |_| panic!(
                "must not change schema without backup"
            ))
            .is_err()
        );
        assert_eq!(
            db.query_row("SELECT id FROM payload", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            7
        );
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}
