use crate::{BaselineEntry, DuetError, EntryKind, PlannedOperation, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempPath;

pub struct Database {
    conn: Connection,
    // SQLite requires advisory byte-range locks, which are not implemented by
    // every filesystem Duet can synchronize with (notably many SFTP mounts).
    // Keep SQLite's live database on a local filesystem and publish committed
    // snapshots back into the portable Target metadata directory.
    local_path: TempPath,
    remote_path: PathBuf,
}

impl Database {
    pub fn open(duet_root: &Path) -> Result<Self> {
        let remote_path = Self::path(duet_root);
        let local_path = tempfile::NamedTempFile::new()
            .map_err(|e| DuetError::io("temporary database", e))?
            .into_temp_path();
        let remote_exists = remote_path
            .try_exists()
            .map_err(|e| DuetError::io(&remote_path, e))?;
        if remote_exists {
            fs::copy(&remote_path, &local_path).map_err(|e| DuetError::io(&remote_path, e))?;
        }
        let conn = Connection::open(&local_path)?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self {
            conn,
            local_path,
            remote_path,
        };
        db.migrate()?;
        if !remote_exists {
            db.persist()?;
        }
        Ok(db)
    }

    /// Atomically replace the portable copy after a committed local change.
    /// The temporary file is created beside the destination so its rename is
    /// atomic on filesystems that support atomic replacement (including
    /// ordinary SFTP mounts).
    fn persist(&self) -> Result<()> {
        let parent = self
            .remote_path
            .parent()
            .ok_or_else(|| DuetError::InvalidDuet(self.remote_path.clone()))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".state-")
            .tempfile_in(parent)
            .map_err(|e| DuetError::io(parent, e))?;
        let bytes = fs::read(&self.local_path).map_err(|e| DuetError::io(&self.local_path, e))?;
        temporary
            .write_all(&bytes)
            .map_err(|e| DuetError::io(&self.remote_path, e))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| DuetError::io(&self.remote_path, e))?;
        temporary
            .persist(&self.remote_path)
            .map_err(|e| DuetError::io(&self.remote_path, e.error))?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS duet (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                last_sync_at TEXT
             );
             INSERT OR IGNORE INTO duet(singleton) VALUES (1);
             CREATE TABLE IF NOT EXISTS entries (
                relative_path TEXT PRIMARY KEY,
                entry_type TEXT NOT NULL,
                source_size INTEGER,
                source_mtime_ns INTEGER,
                duet_size INTEGER,
                duet_mtime_ns INTEGER,
                last_sync_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS transactions (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                error TEXT
             );
             CREATE TABLE IF NOT EXISTS transaction_operations (
                transaction_id TEXT NOT NULL,
                sequence INTEGER NOT NULL,
                relative_path TEXT NOT NULL,
                operation_type TEXT NOT NULL,
                state TEXT NOT NULL,
                PRIMARY KEY(transaction_id, sequence),
                FOREIGN KEY(transaction_id) REFERENCES transactions(id)
             );",
        )?;
        Ok(())
    }

    pub fn entries(&self) -> Result<BTreeMap<PathBuf, BaselineEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT relative_path, entry_type,
                    source_size, source_mtime_ns, duet_size, duet_mtime_ns
             FROM entries ORDER BY relative_path",
        )?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let kind: String = row.get(1)?;
            Ok(BaselineEntry {
                relative_path: PathBuf::from(path),
                kind: match kind.as_str() {
                    "directory" => EntryKind::Directory,
                    "symbolic_link" => EntryKind::SymbolicLink,
                    _ => EntryKind::File,
                },
                source_size: row.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                source_mtime_ns: row.get(3)?,
                duet_size: row.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                duet_mtime_ns: row.get(5)?,
            })
        })?;
        let mut result = BTreeMap::new();
        for row in rows {
            let entry = row?;
            result.insert(entry.relative_path.clone(), entry);
        }
        Ok(result)
    }

    pub fn begin_transaction(&self, operations: &[PlannedOperation]) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO transactions(id, state, started_at) VALUES (?1, 'APPLYING', ?2)",
            params![id, chrono::Utc::now().to_rfc3339()],
        )?;
        for (sequence, op) in operations.iter().enumerate() {
            tx.execute(
                "INSERT INTO transaction_operations
                 (transaction_id, sequence, relative_path, operation_type, state)
                 VALUES (?1, ?2, ?3, ?4, 'PENDING')",
                params![
                    id,
                    sequence as i64,
                    op.relative_path.to_string_lossy(),
                    format!("{:?}", op.action)
                ],
            )?;
        }
        tx.commit()?;
        self.persist()?;
        Ok(id)
    }

    pub fn mark_operation_complete(&self, transaction_id: &str, sequence: usize) -> Result<()> {
        self.conn.execute(
            "UPDATE transaction_operations SET state = 'COMPLETE'
             WHERE transaction_id = ?1 AND sequence = ?2",
            params![transaction_id, sequence as i64],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn complete_transaction(&self, transaction_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE transactions SET state = 'COMPLETE', completed_at = ?2 WHERE id = ?1",
            params![transaction_id, chrono::Utc::now().to_rfc3339()],
        )?;
        self.conn.execute(
            "UPDATE duet SET last_sync_at = ?1 WHERE singleton = 1",
            params![chrono::Utc::now().to_rfc3339()],
        )?;
        self.persist()?;
        Ok(())
    }

    /// Commit the new baseline and the journal completion in one SQLite
    /// transaction. Besides making the state atomic, this avoids one durable
    /// database commit per synchronized file on slow removable storage.
    pub fn finalize_sync(
        &self,
        transaction_id: &str,
        entries: &[BaselineEntry],
        removed_paths: &[PathBuf],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for path in removed_paths {
            tx.execute(
                "DELETE FROM entries WHERE relative_path = ?1",
                params![path.to_string_lossy()],
            )?;
        }
        let now = chrono::Utc::now().to_rfc3339();
        for entry in entries {
            tx.execute(
                "INSERT INTO entries (
                    relative_path, entry_type,
                    source_size, source_mtime_ns, duet_size, duet_mtime_ns, last_sync_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(relative_path) DO UPDATE SET
                    entry_type=excluded.entry_type,
                    source_size=excluded.source_size,
                    source_mtime_ns=excluded.source_mtime_ns,
                    duet_size=excluded.duet_size,
                    duet_mtime_ns=excluded.duet_mtime_ns,
                    last_sync_at=excluded.last_sync_at",
                params![
                    entry.relative_path.to_string_lossy(),
                    match entry.kind {
                        EntryKind::File => "file",
                        EntryKind::Directory => "directory",
                        EntryKind::SymbolicLink => "symbolic_link",
                    },
                    entry.source_size.map(|value| value as i64),
                    entry.source_mtime_ns,
                    entry.duet_size.map(|value| value as i64),
                    entry.duet_mtime_ns,
                    now,
                ],
            )?;
        }
        tx.execute(
            "UPDATE transaction_operations SET state = 'COMPLETE'
             WHERE transaction_id = ?1",
            params![transaction_id],
        )?;
        tx.execute(
            "UPDATE transactions SET state = 'COMPLETE', completed_at = ?2 WHERE id = ?1",
            params![transaction_id, now],
        )?;
        tx.execute(
            "UPDATE duet SET last_sync_at = ?1 WHERE singleton = 1",
            params![now],
        )?;
        tx.commit()?;
        self.persist()?;
        Ok(())
    }

    pub fn fail_transaction(&self, transaction_id: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE transactions SET state = 'INTERRUPTED', error = ?2 WHERE id = ?1",
            params![transaction_id, error],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn has_incomplete_transaction(&self) -> Result<bool> {
        let value: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM transactions WHERE state != 'COMPLETE' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.is_some())
    }

    pub fn upsert_entry(&self, entry: &BaselineEntry) -> Result<()> {
        self.conn.execute(
            "INSERT INTO entries (
                relative_path, entry_type,
                source_size, source_mtime_ns, duet_size, duet_mtime_ns, last_sync_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(relative_path) DO UPDATE SET
                entry_type=excluded.entry_type,
                source_size=excluded.source_size,
                source_mtime_ns=excluded.source_mtime_ns,
                duet_size=excluded.duet_size,
                duet_mtime_ns=excluded.duet_mtime_ns,
                last_sync_at=excluded.last_sync_at",
            params![
                entry.relative_path.to_string_lossy(),
                match entry.kind {
                    EntryKind::File => "file",
                    EntryKind::Directory => "directory",
                    EntryKind::SymbolicLink => "symbolic_link",
                },
                entry.source_size.map(|v| v as i64),
                entry.source_mtime_ns,
                entry.duet_size.map(|v| v as i64),
                entry.duet_mtime_ns,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn remove_entry(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE relative_path = ?1",
            params![path.to_string_lossy()],
        )?;
        self.persist()?;
        Ok(())
    }

    pub fn path(duet_root: &Path) -> PathBuf {
        duet_root.join(".duet/state.sqlite")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_committed_state_to_the_portable_database() {
        let temp = tempfile::tempdir().unwrap();
        let duet_root = temp.path().join("target");
        fs::create_dir_all(duet_root.join(".duet")).unwrap();

        let database = Database::open(&duet_root).unwrap();
        database
            .upsert_entry(&BaselineEntry {
                relative_path: PathBuf::from("note.txt"),
                kind: EntryKind::File,
                source_size: Some(5),
                source_mtime_ns: Some(10),
                duet_size: Some(5),
                duet_mtime_ns: Some(20),
            })
            .unwrap();
        drop(database);

        let reopened = Database::open(&duet_root).unwrap();
        assert!(reopened
            .entries()
            .unwrap()
            .contains_key(Path::new("note.txt")));
    }
}
