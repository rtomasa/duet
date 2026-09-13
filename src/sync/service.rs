use super::{executor, planner, scanner};
use crate::{
    BaselineEntry, ConflictResolution, Database, DuetError, DuetManifest, FileSnapshot,
    ManifestRepository, PlannedOperation, Result, SyncAction, SyncPlan,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct DuetService {
    pub duet_root: PathBuf,
    pub manifest: DuetManifest,
    database: Database,
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub applied: usize,
    pub skipped_conflicts: usize,
    pub recovered_incomplete_transaction: bool,
}

impl DuetService {
    pub fn create(source_root: &Path, duet_root: &Path, name: &str) -> Result<Self> {
        ensure_non_overlapping(source_root, duet_root)?;
        if !source_root.is_dir() {
            return Err(DuetError::SourceUnavailable(source_root.to_path_buf()));
        }
        if duet_root.exists() {
            let mut entries = fs::read_dir(duet_root).map_err(|e| DuetError::io(duet_root, e))?;
            if entries.next().is_some() {
                return Err(DuetError::DestinationNotEmpty(duet_root.to_path_buf()));
            }
        }
        fs::create_dir_all(duet_root).map_err(|e| DuetError::io(duet_root, e))?;
        let manifest = ManifestRepository::create(duet_root, source_root, name)?;
        let database = Database::open(duet_root)?;
        Ok(Self {
            duet_root: duet_root.to_path_buf(),
            manifest,
            database,
        })
    }

    pub fn open(duet_root: &Path) -> Result<Self> {
        let manifest = ManifestRepository::load(duet_root)?;
        let database = Database::open(duet_root)?;
        ensure_non_overlapping(&manifest.source.last_known_path, duet_root)?;
        Ok(Self {
            duet_root: duet_root.to_path_buf(),
            manifest,
            database,
        })
    }

    pub fn rebind_source(&mut self, source_root: &Path) -> Result<()> {
        ensure_non_overlapping(source_root, &self.duet_root)?;
        if !source_root.is_dir() {
            return Err(DuetError::SourceUnavailable(source_root.to_path_buf()));
        }
        self.manifest = ManifestRepository::rebind_source(&self.duet_root, source_root.into())?;
        Ok(())
    }

    pub fn compare(&self) -> Result<SyncPlan> {
        self.compare_with_progress_and_cancel(false, |_, _| {}, || false)
    }

    pub fn compare_with_progress_and_cancel<F, C>(
        &self,
        skip_hidden_files: bool,
        progress: F,
        is_cancelled: C,
    ) -> Result<SyncPlan>
    where
        F: Fn(u64, u64) + Sync,
        C: Fn() -> bool + Sync,
    {
        let mut baseline = self.database.entries()?;
        if skip_hidden_files {
            baseline.retain(|path, _| !scanner::is_hidden_path(path));
        }
        let source = scanner::scan_with_progress_and_cancel(
            &self.manifest.source.last_known_path,
            skip_hidden_files,
            &progress,
            &is_cancelled,
        )?;
        let duet = scanner::scan_with_progress_and_cancel(
            &self.duet_root,
            skip_hidden_files,
            &progress,
            &is_cancelled,
        )?;
        Ok(planner::build_plan(&baseline, source, duet))
    }

    pub fn synchronize(
        &self,
        plan: &SyncPlan,
        resolutions: &BTreeMap<PathBuf, ConflictResolution>,
    ) -> Result<SyncOutcome> {
        self.synchronize_with_progress(plan, resolutions, |_, _, _, _| {})
    }

    pub fn synchronize_with_progress<F>(
        &self,
        plan: &SyncPlan,
        resolutions: &BTreeMap<PathBuf, ConflictResolution>,
        progress: F,
    ) -> Result<SyncOutcome>
    where
        F: FnMut(&PlannedOperation, u64, u64, bool),
    {
        self.synchronize_with_progress_and_cancel(plan, resolutions, progress, || false)
    }

    pub fn synchronize_with_progress_and_cancel<F, C>(
        &self,
        plan: &SyncPlan,
        resolutions: &BTreeMap<PathBuf, ConflictResolution>,
        mut progress: F,
        is_cancelled: C,
    ) -> Result<SyncOutcome>
    where
        F: FnMut(&PlannedOperation, u64, u64, bool),
        C: Fn() -> bool,
    {
        let recovered = self.database.has_incomplete_transaction()?;
        let _lock = executor::MutationLock::acquire(&self.duet_root)?;
        let mut operations: Vec<_> = plan
            .operations
            .iter()
            .filter(|op| !matches!(op.action, SyncAction::None))
            .cloned()
            .collect();
        let mut skipped = 0;
        for conflict in &plan.conflicts {
            match resolutions
                .get(&conflict.operation.relative_path)
                .copied()
                .unwrap_or(ConflictResolution::Skip)
            {
                ConflictResolution::Skip => skipped += 1,
                resolution => operations.push(resolve_conflict(conflict, resolution)?),
            }
        }
        let operations = executor::ordered(&operations);
        let transaction = self.database.begin_transaction(&operations)?;
        let mut updated_entries = Vec::new();
        let mut removed_paths = Vec::new();
        for operation in &operations {
            let expected = match checked_origin_snapshot(plan, operation) {
                Ok(expected) => expected,
                Err(error) => {
                    let _ = self
                        .database
                        .fail_transaction(&transaction, &error.to_string());
                    return Err(error);
                }
            };
            let copied = match executor::apply_one_with_progress(
                &self.manifest.source.last_known_path,
                &self.duet_root,
                operation,
                expected,
                &mut |completed, total| progress(operation, completed, total, false),
                &is_cancelled,
            ) {
                Ok(copied) => copied,
                Err(error) => {
                    let _ = self
                        .database
                        .fail_transaction(&transaction, &error.to_string());
                    return Err(error);
                }
            };
            match baseline_change(operation, copied, plan) {
                Ok(Some(entry)) => updated_entries.push(entry),
                Ok(None) => removed_paths.push(operation.relative_path.clone()),
                Err(error) => {
                    let _ = self
                        .database
                        .fail_transaction(&transaction, &error.to_string());
                    return Err(error);
                }
            };
            // The filesystem operation itself is complete. Reporting this here
            // lets the UI retire rows steadily instead of receiving one large
            // burst only after the database commit.
            progress(operation, 1, 1, true);
        }
        if is_cancelled() {
            let error = DuetError::Cancelled;
            let _ = self
                .database
                .fail_transaction(&transaction, &error.to_string());
            return Err(error);
        }
        if let Err(error) =
            self.database
                .finalize_sync(&transaction, &updated_entries, &removed_paths)
        {
            let _ = self
                .database
                .fail_transaction(&transaction, &error.to_string());
            return Err(error);
        }
        Ok(SyncOutcome {
            applied: operations.len(),
            skipped_conflicts: skipped,
            recovered_incomplete_transaction: recovered,
        })
    }
}

fn checked_origin_snapshot<'a>(
    plan: &'a SyncPlan,
    operation: &PlannedOperation,
) -> Result<Option<&'a FileSnapshot>> {
    let snapshot = match operation.action {
        SyncAction::SourceToDuet => plan.source_snapshots.get(&operation.relative_path),
        SyncAction::DuetToSource => plan.duet_snapshots.get(&operation.relative_path),
        _ => return Ok(None),
    };
    snapshot.map(Some).ok_or_else(|| {
        DuetError::Other(anyhow::anyhow!(
            "The checked copy of {} is no longer available",
            operation.relative_path.display()
        ))
    })
}

fn baseline_change(
    operation: &PlannedOperation,
    copied: Option<executor::CopiedMetadata>,
    plan: &SyncPlan,
) -> Result<Option<BaselineEntry>> {
    let path = &operation.relative_path;
    let entry = match operation.action {
        SyncAction::SourceToDuet | SyncAction::DuetToSource => {
            let copied = copied.ok_or_else(|| {
                DuetError::Other(anyhow::anyhow!(
                    "No copy metadata was recorded for {}",
                    path.display()
                ))
            })?;
            let (source_size, source_mtime_ns, duet_size, duet_mtime_ns) =
                if operation.action == SyncAction::SourceToDuet {
                    (
                        copied.from_size,
                        copied.from_mtime_ns,
                        copied.to_size,
                        copied.to_mtime_ns,
                    )
                } else {
                    (
                        copied.to_size,
                        copied.to_mtime_ns,
                        copied.from_size,
                        copied.from_mtime_ns,
                    )
                };
            BaselineEntry {
                relative_path: path.clone(),
                kind: operation.kind,
                source_size: Some(source_size),
                source_mtime_ns: Some(source_mtime_ns),
                duet_size: Some(duet_size),
                duet_mtime_ns: Some(duet_mtime_ns),
            }
        }
        SyncAction::RecordBaseline => {
            let source = plan.source_snapshots.get(path).ok_or_else(|| {
                DuetError::Other(anyhow::anyhow!(
                    "No Source metadata was recorded for {}",
                    path.display()
                ))
            })?;
            let duet = plan.duet_snapshots.get(path).ok_or_else(|| {
                DuetError::Other(anyhow::anyhow!(
                    "No Target metadata was recorded for {}",
                    path.display()
                ))
            })?;
            BaselineEntry {
                relative_path: path.clone(),
                kind: operation.kind,
                source_size: Some(source.size),
                source_mtime_ns: Some(source.mtime_ns),
                duet_size: Some(duet.size),
                duet_mtime_ns: Some(duet.mtime_ns),
            }
        }
        SyncAction::DeleteSource | SyncAction::DeleteDuet | SyncAction::RemoveBaseline => {
            return Ok(None)
        }
        SyncAction::None => return Ok(None),
        SyncAction::Conflict => {
            return Err(DuetError::UnresolvedConflict(path.clone()));
        }
    };
    Ok(Some(entry))
}

fn resolve_conflict(
    conflict: &crate::Conflict,
    resolution: ConflictResolution,
) -> Result<PlannedOperation> {
    let operation = &conflict.operation;
    let mut result = operation.clone();
    result.action = match resolution {
        ConflictResolution::KeepSource => {
            result.kind = conflict
                .source
                .as_ref()
                .ok_or_else(|| DuetError::UnresolvedConflict(operation.relative_path.clone()))?
                .kind;
            SyncAction::SourceToDuet
        }
        ConflictResolution::KeepDuet => {
            result.kind = conflict
                .duet
                .as_ref()
                .ok_or_else(|| DuetError::UnresolvedConflict(operation.relative_path.clone()))?
                .kind;
            SyncAction::DuetToSource
        }
        ConflictResolution::AcceptDeletion => {
            match (operation.source_state, operation.duet_state) {
                (crate::ChangeState::Deleted, _) => {
                    result.kind = conflict
                        .duet
                        .as_ref()
                        .map(|s| s.kind)
                        .unwrap_or(result.kind);
                    SyncAction::DeleteDuet
                }
                (_, crate::ChangeState::Deleted) => {
                    result.kind = conflict
                        .source
                        .as_ref()
                        .map(|s| s.kind)
                        .unwrap_or(result.kind);
                    SyncAction::DeleteSource
                }
                _ => {
                    return Err(DuetError::Other(anyhow::anyhow!(
                        "There is no deletion to accept for {}",
                        operation.relative_path.display()
                    )))
                }
            }
        }
        ConflictResolution::Skip => {
            return Err(DuetError::UnresolvedConflict(
                operation.relative_path.clone(),
            ))
        }
    };
    Ok(result)
}

fn ensure_non_overlapping(source: &Path, duet: &Path) -> Result<()> {
    let source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let duet = duet.canonicalize().unwrap_or_else(|_| duet.to_path_buf());
    if source == duet || source.starts_with(&duet) || duet.starts_with(&source) {
        return Err(DuetError::OverlappingRoots);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn initial_sync_copies_nested_files_and_empty_directories() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(source.join("empty")).unwrap();
        write(&source.join("docs/note.txt"), "hello");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let plan = service.compare().unwrap();
        assert_eq!(plan.conflicts.len(), 0);
        service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(
            fs::read_to_string(duet.join("docs/note.txt")).unwrap(),
            "hello"
        );
        assert!(duet.join("empty").is_dir());
        assert!(!source.join(".duet").exists());
    }

    #[test]
    fn skipping_hidden_files_leaves_previously_synced_hidden_files_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("visible.txt"), "visible");
        write(&source.join(".hidden.txt"), "hidden");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let plan = service.compare().unwrap();
        service.synchronize(&plan, &BTreeMap::new()).unwrap();

        let plan = service
            .compare_with_progress_and_cancel(true, |_, _| {}, || false)
            .unwrap();

        assert!(!plan
            .operations
            .iter()
            .any(|operation| operation.relative_path == Path::new(".hidden.txt")));
        assert!(duet.join(".hidden.txt").is_file());
    }

    #[test]
    fn matching_untracked_copies_are_adopted_without_being_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("already-copied.txt"), "same contents");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        fs::copy(
            source.join("already-copied.txt"),
            duet.join("already-copied.txt"),
        )
        .unwrap();
        let source_mtime = fs::metadata(source.join("already-copied.txt"))
            .unwrap()
            .modified()
            .unwrap();
        filetime::set_file_mtime(
            duet.join("already-copied.txt"),
            filetime::FileTime::from_system_time(source_mtime),
        )
        .unwrap();

        let plan = service.compare().unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.operations.len(), 1);
        assert_eq!(plan.operations[0].action, SyncAction::RecordBaseline);
        service.synchronize(&plan, &BTreeMap::new()).unwrap();

        let settled = service.compare().unwrap();
        assert_eq!(settled.actionable_count(), 0);
        assert!(settled.conflicts.is_empty());
    }

    #[test]
    fn synchronization_reports_byte_progress_and_completion() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("progress.txt"), "progress");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let plan = service.compare().unwrap();
        let mut events = Vec::new();

        service
            .synchronize_with_progress(
                &plan,
                &BTreeMap::new(),
                |operation, completed, total, finished| {
                    events.push((operation.relative_path.clone(), completed, total, finished));
                },
            )
            .unwrap();

        assert!(events.iter().any(|(path, completed, total, finished)| {
            path == Path::new("progress.txt") && !finished && *completed == *total && *total > 0
        }));
        assert!(events
            .iter()
            .any(|(path, _, _, finished)| path == Path::new("progress.txt") && *finished));
        let copied = events
            .iter()
            .position(|(path, completed, total, finished)| {
                path == Path::new("progress.txt") && !finished && *completed == *total && *total > 0
            })
            .unwrap();
        let finished = events
            .iter()
            .position(|(path, _, _, finished)| path == Path::new("progress.txt") && *finished)
            .unwrap();
        assert!(copied < finished);
    }

    #[test]
    fn synchronization_can_be_stopped_during_a_file_copy() {
        use std::cell::Cell;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("large.bin"), vec![7_u8; 3 * 1024 * 1024]).unwrap();
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let plan = service.compare().unwrap();
        let cancelled = Cell::new(false);

        let result = service.synchronize_with_progress_and_cancel(
            &plan,
            &BTreeMap::new(),
            |operation, completed, _, finished| {
                if operation.relative_path == Path::new("large.bin") && completed > 0 && !finished {
                    cancelled.set(true);
                }
            },
            || cancelled.get(),
        );

        assert!(matches!(result, Err(DuetError::Cancelled)));
        assert!(!duet.join("large.bin").exists());
    }

    #[test]
    fn empty_file_copy_reports_determinate_completion() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("empty.bin"), []).unwrap();
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let plan = service.compare().unwrap();
        let mut events = Vec::new();

        service
            .synchronize_with_progress(
                &plan,
                &BTreeMap::new(),
                |operation, completed, total, finished| {
                    if operation.relative_path == Path::new("empty.bin") {
                        events.push((completed, total, finished));
                    }
                },
            )
            .unwrap();

        assert!(events
            .iter()
            .any(|(completed, total, finished)| *completed == 1 && *total == 1 && !finished));
        assert!(events.iter().any(|(_, _, finished)| *finished));
    }

    #[test]
    fn changes_flow_in_both_directions_and_conflicts_are_not_silent() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("a.txt"), "base");
        write(&source.join("b.txt"), "base");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let plan = service.compare().unwrap();
        service.synchronize(&plan, &BTreeMap::new()).unwrap();

        write(&source.join("a.txt"), "source edit");
        write(&duet.join("b.txt"), "portable edit");
        let plan = service.compare().unwrap();
        service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(
            fs::read_to_string(duet.join("a.txt")).unwrap(),
            "source edit"
        );
        assert_eq!(
            fs::read_to_string(source.join("b.txt")).unwrap(),
            "portable edit"
        );

        write(&source.join("a.txt"), "left");
        write(&duet.join("a.txt"), "right");
        let plan = service.compare().unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        let outcome = service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(outcome.skipped_conflicts, 1);
        assert_eq!(fs::read_to_string(source.join("a.txt")).unwrap(), "left");
        assert_eq!(fs::read_to_string(duet.join("a.txt")).unwrap(), "right");
    }

    #[test]
    fn deletion_is_synchronized() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("remove.txt"), "data");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let first = service.compare().unwrap();
        service.synchronize(&first, &BTreeMap::new()).unwrap();
        fs::remove_file(source.join("remove.txt")).unwrap();
        let second = service.compare().unwrap();
        assert!(second.has_deletions());
        service.synchronize(&second, &BTreeMap::new()).unwrap();
        assert!(!duet.join("remove.txt").exists());
    }

    #[test]
    fn explicit_resolution_replaces_both_sides_and_sets_a_new_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("conflict.txt"), "base");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let first = service.compare().unwrap();
        service.synchronize(&first, &BTreeMap::new()).unwrap();
        write(&source.join("conflict.txt"), "source wins");
        write(&duet.join("conflict.txt"), "portable loses");

        let plan = service.compare().unwrap();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(
            PathBuf::from("conflict.txt"),
            ConflictResolution::KeepSource,
        );
        service.synchronize(&plan, &resolutions).unwrap();
        assert_eq!(
            fs::read_to_string(duet.join("conflict.txt")).unwrap(),
            "source wins"
        );
        let settled = service.compare().unwrap();
        assert_eq!(settled.actionable_count(), 0);
        assert!(settled.conflicts.is_empty());
    }

    #[test]
    fn deletion_conflict_can_accept_the_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("choice.txt"), "base");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let first = service.compare().unwrap();
        service.synchronize(&first, &BTreeMap::new()).unwrap();
        fs::remove_file(source.join("choice.txt")).unwrap();
        write(&duet.join("choice.txt"), "edited while away");

        let plan = service.compare().unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        let mut resolutions = BTreeMap::new();
        resolutions.insert(
            PathBuf::from("choice.txt"),
            ConflictResolution::AcceptDeletion,
        );
        service.synchronize(&plan, &resolutions).unwrap();
        assert!(!duet.join("choice.txt").exists());
        assert!(service.compare().unwrap().conflicts.is_empty());
    }

    #[test]
    fn refuses_to_initialize_over_existing_content() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&duet).unwrap();
        write(&duet.join("important.txt"), "do not overwrite");
        assert!(matches!(
            DuetService::create(&source, &duet, "Bad"),
            Err(DuetError::DestinationNotEmpty(_))
        ));
        assert_eq!(
            fs::read_to_string(duet.join("important.txt")).unwrap(),
            "do not overwrite"
        );
    }

    #[test]
    fn rejects_nested_roots_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        assert!(matches!(
            DuetService::create(&source, &source.join("portable"), "Bad"),
            Err(DuetError::OverlappingRoots)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", source.join("escape")).unwrap();
            let duet = temp.path().join("portable");
            let service = DuetService::create(&source, &duet, "Test").unwrap();
            assert!(matches!(
                service.compare(),
                Err(DuetError::UnsupportedSymlink(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn synchronization_only_rechecks_paths_affected_by_the_plan() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let duet = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("changed.txt"), "before");
        let service = DuetService::create(&source, &duet, "Test").unwrap();
        let initial = service.compare().unwrap();
        service.synchronize(&initial, &BTreeMap::new()).unwrap();

        write(&source.join("changed.txt"), "after");
        let plan = service.compare().unwrap();
        std::os::unix::fs::symlink("/tmp", source.join("appeared-after-check")).unwrap();

        service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(
            fs::read_to_string(duet.join("changed.txt")).unwrap(),
            "after"
        );
    }
}
