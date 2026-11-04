//! SQLite connection opening with automated corruption recovery and backup preservation.
//! Never silently deletes corrupted databases; copies them to timestamped `.bak` files.

use chrono::Utc;
use gm_core::error::{GmError, Result};
use rusqlite::Connection;
use std::path::Path;
use tracing::warn;

/// Perform a non-destructive, read-only integrity check on a SQLite database.
pub fn check_sqlite_integrity(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| GmError::Platform(format!("Failed to open SQLite database for check: {e}")))?;
    let check: String = conn
        .prepare("PRAGMA quick_check(1)")
        .and_then(|mut stmt| stmt.query_row([], |r| r.get(0)))
        .map_err(|e| GmError::Platform(format!("Integrity check query failed: {e}")))?;
    if check.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(GmError::Platform(format!(
            "Integrity check failed: {check}"
        )))
    }
}

pub fn open_sqlite_with_recovery(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    if path.exists() {
        let is_healthy = match Connection::open(path) {
            Ok(c) => {
                let check_res: rusqlite::Result<String> = c
                    .prepare("PRAGMA quick_check(1)")
                    .and_then(|mut stmt| stmt.query_row([], |r| r.get(0)));
                match check_res {
                    Ok(res) => res.eq_ignore_ascii_case("ok"),
                    Err(_) => false,
                }
            }
            Err(_) => false,
        };

        if !is_healthy {
            let ts = Utc::now().timestamp();
            let backup_path = path.with_extension(format!("corrupt.{ts}.bak"));
            warn!(
                "SQLite database at {:?} failed integrity check; preserving corrupt data at {:?} before reinitializing",
                path, backup_path
            );
            // Always preserve user data before touching existing corrupted file.
            // If backup preservation fails, refuse to destroy original data!
            let preserved = if std::fs::rename(path, &backup_path).is_ok() {
                true
            } else if std::fs::copy(path, &backup_path).is_ok() {
                let _ = std::fs::remove_file(path);
                true
            } else {
                false
            };

            if !preserved {
                return Err(GmError::Platform(format!(
                    "SQLite database at {:?} failed integrity check and could not be preserved to {:?}; refusing to destroy original file",
                    path, backup_path
                )));
            }

            let wal = format!("{}-wal", path.display());
            let wal_path = Path::new(&wal);
            if wal_path.exists() {
                let wal_bak = path.with_extension(format!("corrupt.{ts}.wal.bak"));
                if std::fs::rename(wal_path, &wal_bak).is_err()
                    && std::fs::copy(wal_path, &wal_bak).is_ok()
                {
                    let _ = std::fs::remove_file(wal_path);
                }
            }

            let shm = format!("{}-shm", path.display());
            let shm_path = Path::new(&shm);
            if shm_path.exists() {
                let shm_bak = path.with_extension(format!("corrupt.{ts}.shm.bak"));
                if std::fs::rename(shm_path, &shm_bak).is_err()
                    && std::fs::copy(shm_path, &shm_bak).is_ok()
                {
                    let _ = std::fs::remove_file(shm_path);
                }
            }
        }
    }

    Connection::open(path).map_err(|e| {
        GmError::Platform(format!("Failed to open SQLite database at {:?}: {e}", path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_corrupt_sqlite_preserved_and_reinitialized() {
        let dir = std::env::temp_dir().join(format!("gm-corrupt-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("test.sqlite");

        // Write corrupt garbage to simulate unexpected corruption / truncated write
        let corrupt_data = b"NOT_A_VALID_SQLITE_HEADER_GARBAGE_DATA";
        std::fs::write(&db_path, corrupt_data).unwrap();

        // open_sqlite_with_recovery must succeed by archiving corrupted file and opening fresh DB
        let conn = open_sqlite_with_recovery(&db_path).unwrap();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY);", [])
            .unwrap();
        drop(conn);

        // Verify that a .bak file was created containing the original corrupt data
        let mut found_bak = false;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("test.corrupt.") && name.ends_with(".bak") {
                let contents = std::fs::read(entry.path()).unwrap();
                assert_eq!(contents, corrupt_data);
                found_bak = true;
            }
        }
        assert!(found_bak, "Expected backup of corrupt DB to be preserved");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
