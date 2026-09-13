use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DuetManifest {
    pub format_version: u32,
    pub duet_id: uuid::Uuid,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub source: SourceLocator,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceLocator {
    pub last_known_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub relative_path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineEntry {
    pub relative_path: PathBuf,
    pub kind: EntryKind,
    pub source_size: Option<u64>,
    pub source_mtime_ns: Option<i64>,
    pub duet_size: Option<u64>,
    pub duet_mtime_ns: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeState {
    Unchanged,
    Modified,
    Deleted,
    Created,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncAction {
    None,
    SourceToDuet,
    DuetToSource,
    DeleteSource,
    DeleteDuet,
    RemoveBaseline,
    /// Both untracked copies already match, so only the synchronization
    /// baseline needs to be recorded.
    RecordBaseline,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictResolution {
    KeepSource,
    KeepDuet,
    AcceptDeletion,
    Skip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedOperation {
    pub relative_path: PathBuf,
    pub kind: EntryKind,
    pub source_state: ChangeState,
    pub duet_state: ChangeState,
    pub action: SyncAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub operation: PlannedOperation,
    pub source: Option<FileSnapshot>,
    pub duet: Option<FileSnapshot>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncPlan {
    pub operations: Vec<PlannedOperation>,
    pub conflicts: Vec<Conflict>,
    pub source_snapshots: BTreeMap<PathBuf, FileSnapshot>,
    pub duet_snapshots: BTreeMap<PathBuf, FileSnapshot>,
}

impl SyncPlan {
    pub fn actionable_count(&self) -> usize {
        self.operations
            .iter()
            .filter(|op| !matches!(op.action, SyncAction::None))
            .count()
    }

    pub fn has_deletions(&self) -> bool {
        self.operations
            .iter()
            .any(|op| matches!(op.action, SyncAction::DeleteSource | SyncAction::DeleteDuet))
    }
}
