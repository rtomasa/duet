use crate::{BaselineEntry, EntryKind, PlannedOperation, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(briefcase_root: &Path) -> Result<Self> {
        let path = briefcase_root.join(".briefcase/state.sqlite");
        let conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "DELETE")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS briefcase (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                last_sync_at TEXT
             );
             INSERT OR IGNORE INTO briefcase(singleton) VALUES (1);
             CREATE TABLE IF NOT EXISTS entries (
                relative_path TEXT PRIMARY KEY,
                entry_type TEXT NOT NULL,
                baseline_hash TEXT,
                source_size INTEGER,
                source_mtime_ns INTEGER,
                briefcase_size INTEGER,
                briefcase_mtime_ns INTEGER,
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
            "SELECT relative_path, entry_type, baseline_hash,
                    source_size, source_mtime_ns, briefcase_size, briefcase_mtime_ns
             FROM entries ORDER BY relative_path",
        )?;
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let kind: String = row.get(1)?;
            Ok(BaselineEntry {
                relative_path: PathBuf::from(path),
                kind: if kind == "directory" {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
                baseline_hash: row.get(2)?,
                source_size: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                source_mtime_ns: row.get(4)?,
                briefcase_size: row.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                briefcase_mtime_ns: row.get(6)?,
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
        Ok(id)
    }

    pub fn mark_operation_complete(&self, transaction_id: &str, sequence: usize) -> Result<()> {
        self.conn.execute(
            "UPDATE transaction_operations SET state = 'COMPLETE'
             WHERE transaction_id = ?1 AND sequence = ?2",
            params![transaction_id, sequence as i64],
        )?;
        Ok(())
    }

    pub fn complete_transaction(&self, transaction_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE transactions SET state = 'COMPLETE', completed_at = ?2 WHERE id = ?1",
            params![transaction_id, chrono::Utc::now().to_rfc3339()],
        )?;
        self.conn.execute(
            "UPDATE briefcase SET last_sync_at = ?1 WHERE singleton = 1",
            params![chrono::Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn fail_transaction(&self, transaction_id: &str, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE transactions SET state = 'INTERRUPTED', error = ?2 WHERE id = ?1",
            params![transaction_id, error],
        )?;
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
                relative_path, entry_type, baseline_hash,
                source_size, source_mtime_ns, briefcase_size, briefcase_mtime_ns, last_sync_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(relative_path) DO UPDATE SET
                entry_type=excluded.entry_type,
                baseline_hash=excluded.baseline_hash,
                source_size=excluded.source_size,
                source_mtime_ns=excluded.source_mtime_ns,
                briefcase_size=excluded.briefcase_size,
                briefcase_mtime_ns=excluded.briefcase_mtime_ns,
                last_sync_at=excluded.last_sync_at",
            params![
                entry.relative_path.to_string_lossy(),
                match entry.kind {
                    EntryKind::File => "file",
                    EntryKind::Directory => "directory",
                },
                entry.baseline_hash,
                entry.source_size.map(|v| v as i64),
                entry.source_mtime_ns,
                entry.briefcase_size.map(|v| v as i64),
                entry.briefcase_mtime_ns,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn remove_entry(&self, path: &Path) -> Result<()> {
        self.conn.execute(
            "DELETE FROM entries WHERE relative_path = ?1",
            params![path.to_string_lossy()],
        )?;
        Ok(())
    }

    pub fn path(briefcase_root: &Path) -> PathBuf {
        briefcase_root.join(".briefcase/state.sqlite")
    }
}
