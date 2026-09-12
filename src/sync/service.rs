use super::{executor, planner, scanner};
use crate::{
    BaselineEntry, BriefcaseError, BriefcaseManifest, ConflictResolution, Database,
    ManifestRepository, PlannedOperation, Result, SyncAction, SyncPlan,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub use super::scanner::ScanMode;

pub struct BriefcaseService {
    pub briefcase_root: PathBuf,
    pub manifest: BriefcaseManifest,
    database: Database,
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub applied: usize,
    pub skipped_conflicts: usize,
    pub recovered_incomplete_transaction: bool,
}

impl BriefcaseService {
    pub fn create(source_root: &Path, briefcase_root: &Path, name: &str) -> Result<Self> {
        ensure_non_overlapping(source_root, briefcase_root)?;
        if !source_root.is_dir() {
            return Err(BriefcaseError::SourceUnavailable(source_root.to_path_buf()));
        }
        if briefcase_root.exists() {
            let mut entries =
                fs::read_dir(briefcase_root).map_err(|e| BriefcaseError::io(briefcase_root, e))?;
            if entries.next().is_some() {
                return Err(BriefcaseError::DestinationNotEmpty(
                    briefcase_root.to_path_buf(),
                ));
            }
        }
        fs::create_dir_all(briefcase_root).map_err(|e| BriefcaseError::io(briefcase_root, e))?;
        let manifest = ManifestRepository::create(briefcase_root, source_root, name)?;
        let database = Database::open(briefcase_root)?;
        Ok(Self {
            briefcase_root: briefcase_root.to_path_buf(),
            manifest,
            database,
        })
    }

    pub fn open(briefcase_root: &Path) -> Result<Self> {
        let manifest = ManifestRepository::load(briefcase_root)?;
        let database = Database::open(briefcase_root)?;
        ensure_non_overlapping(&manifest.source.last_known_path, briefcase_root)?;
        Ok(Self {
            briefcase_root: briefcase_root.to_path_buf(),
            manifest,
            database,
        })
    }

    pub fn rebind_source(&mut self, source_root: &Path) -> Result<()> {
        ensure_non_overlapping(source_root, &self.briefcase_root)?;
        if !source_root.is_dir() {
            return Err(BriefcaseError::SourceUnavailable(source_root.to_path_buf()));
        }
        self.manifest =
            ManifestRepository::rebind_source(&self.briefcase_root, source_root.into())?;
        Ok(())
    }

    pub fn compare(&self, mode: ScanMode) -> Result<SyncPlan> {
        let baseline = self.database.entries()?;
        let source = scanner::scan(
            &self.manifest.source.last_known_path,
            scanner::ScanSide::Source,
            &baseline,
            mode,
        )?;
        let briefcase = scanner::scan(
            &self.briefcase_root,
            scanner::ScanSide::Briefcase,
            &baseline,
            mode,
        )?;
        Ok(planner::build_plan(&baseline, source, briefcase))
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
        let _lock = executor::MutationLock::acquire(&self.briefcase_root)?;
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
        for (sequence, operation) in operations.iter().enumerate() {
            if let Err(error) = executor::apply_one_with_progress(
                &self.manifest.source.last_known_path,
                &self.briefcase_root,
                operation,
                &mut |completed, total| progress(operation, completed, total, false),
                &is_cancelled,
            ) {
                let _ = self
                    .database
                    .fail_transaction(&transaction, &error.to_string());
                return Err(error);
            }
            self.database
                .mark_operation_complete(&transaction, sequence)?;
        }
        self.refresh_baseline_for(&operations)?;
        self.database.complete_transaction(&transaction)?;
        for operation in &operations {
            progress(operation, 1, 1, true);
        }
        Ok(SyncOutcome {
            applied: operations.len(),
            skipped_conflicts: skipped,
            recovered_incomplete_transaction: recovered,
        })
    }

    fn refresh_baseline_for(&self, operations: &[PlannedOperation]) -> Result<()> {
        let empty = BTreeMap::new();
        let source = scanner::scan(
            &self.manifest.source.last_known_path,
            scanner::ScanSide::Source,
            &empty,
            ScanMode::Verified,
        )?;
        let briefcase = scanner::scan(
            &self.briefcase_root,
            scanner::ScanSide::Briefcase,
            &empty,
            ScanMode::Verified,
        )?;
        for operation in operations {
            let path = &operation.relative_path;
            match (source.get(path), briefcase.get(path)) {
                (None, None) => self.database.remove_entry(path)?,
                (Some(left), Some(right)) if left.kind == right.kind && left.hash == right.hash => {
                    self.database.upsert_entry(&BaselineEntry {
                        relative_path: path.clone(),
                        kind: left.kind,
                        baseline_hash: left.hash.clone(),
                        source_size: Some(left.size),
                        source_mtime_ns: Some(left.mtime_ns),
                        briefcase_size: Some(right.size),
                        briefcase_mtime_ns: Some(right.mtime_ns),
                    })?;
                }
                _ => {
                    return Err(BriefcaseError::Other(anyhow::anyhow!(
                        "The operation on {} did not produce equivalent copies",
                        path.display()
                    )))
                }
            }
        }
        Ok(())
    }
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
                .ok_or_else(|| BriefcaseError::UnresolvedConflict(operation.relative_path.clone()))?
                .kind;
            SyncAction::SourceToBriefcase
        }
        ConflictResolution::KeepBriefcase => {
            result.kind = conflict
                .briefcase
                .as_ref()
                .ok_or_else(|| BriefcaseError::UnresolvedConflict(operation.relative_path.clone()))?
                .kind;
            SyncAction::BriefcaseToSource
        }
        ConflictResolution::AcceptDeletion => {
            match (operation.source_state, operation.briefcase_state) {
                (crate::ChangeState::Deleted, _) => {
                    result.kind = conflict
                        .briefcase
                        .as_ref()
                        .map(|s| s.kind)
                        .unwrap_or(result.kind);
                    SyncAction::DeleteBriefcase
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
                    return Err(BriefcaseError::Other(anyhow::anyhow!(
                        "There is no deletion to accept for {}",
                        operation.relative_path.display()
                    )))
                }
            }
        }
        ConflictResolution::Skip => {
            return Err(BriefcaseError::UnresolvedConflict(
                operation.relative_path.clone(),
            ))
        }
    };
    Ok(result)
}

fn ensure_non_overlapping(source: &Path, briefcase: &Path) -> Result<()> {
    let source = source
        .canonicalize()
        .unwrap_or_else(|_| source.to_path_buf());
    let briefcase = briefcase
        .canonicalize()
        .unwrap_or_else(|_| briefcase.to_path_buf());
    if source == briefcase || source.starts_with(&briefcase) || briefcase.starts_with(&source) {
        return Err(BriefcaseError::OverlappingRoots);
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
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(source.join("empty")).unwrap();
        write(&source.join("docs/note.txt"), "hello");
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let plan = service.compare(ScanMode::Verified).unwrap();
        assert_eq!(plan.conflicts.len(), 0);
        service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(
            fs::read_to_string(briefcase.join("docs/note.txt")).unwrap(),
            "hello"
        );
        assert!(briefcase.join("empty").is_dir());
        assert!(!source.join(".briefcase").exists());
    }

    #[test]
    fn synchronization_reports_byte_progress_and_completion() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("progress.txt"), "progress");
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let plan = service.compare(ScanMode::Verified).unwrap();
        let mut events = Vec::new();

        service
            .synchronize_with_progress(
                &plan,
                &BTreeMap::new(),
                |operation, completed, total, finished| {
                    events.push((
                        operation.relative_path.clone(),
                        completed,
                        total,
                        finished,
                    ));
                },
            )
            .unwrap();

        assert!(events.iter().any(|(path, completed, total, finished)| {
            path == Path::new("progress.txt")
                && !finished
                && *completed == *total
                && *total > 0
        }));
        assert!(events
            .iter()
            .any(|(path, _, _, finished)| path == Path::new("progress.txt") && *finished));
        let copied = events
            .iter()
            .position(|(path, completed, total, finished)| {
                path == Path::new("progress.txt")
                    && !finished
                    && *completed == *total
                    && *total > 0
            })
            .unwrap();
        let finished = events
            .iter()
            .position(|(path, _, _, finished)| {
                path == Path::new("progress.txt") && *finished
            })
            .unwrap();
        assert!(copied < finished);
    }

    #[test]
    fn synchronization_can_be_stopped_during_a_file_copy() {
        use std::cell::Cell;

        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("large.bin"), vec![7_u8; 3 * 1024 * 1024]).unwrap();
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let plan = service.compare(ScanMode::Verified).unwrap();
        let cancelled = Cell::new(false);

        let result = service.synchronize_with_progress_and_cancel(
            &plan,
            &BTreeMap::new(),
            |operation, completed, _, finished| {
                if operation.relative_path == Path::new("large.bin")
                    && completed > 0
                    && !finished
                {
                    cancelled.set(true);
                }
            },
            || cancelled.get(),
        );

        assert!(matches!(result, Err(BriefcaseError::Cancelled)));
        assert!(!briefcase.join("large.bin").exists());
    }

    #[test]
    fn changes_flow_in_both_directions_and_conflicts_are_not_silent() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("a.txt"), "base");
        write(&source.join("b.txt"), "base");
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let plan = service.compare(ScanMode::Verified).unwrap();
        service.synchronize(&plan, &BTreeMap::new()).unwrap();

        write(&source.join("a.txt"), "source edit");
        write(&briefcase.join("b.txt"), "portable edit");
        let plan = service.compare(ScanMode::Verified).unwrap();
        service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(
            fs::read_to_string(briefcase.join("a.txt")).unwrap(),
            "source edit"
        );
        assert_eq!(
            fs::read_to_string(source.join("b.txt")).unwrap(),
            "portable edit"
        );

        write(&source.join("a.txt"), "left");
        write(&briefcase.join("a.txt"), "right");
        let plan = service.compare(ScanMode::Verified).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        let outcome = service.synchronize(&plan, &BTreeMap::new()).unwrap();
        assert_eq!(outcome.skipped_conflicts, 1);
        assert_eq!(fs::read_to_string(source.join("a.txt")).unwrap(), "left");
        assert_eq!(
            fs::read_to_string(briefcase.join("a.txt")).unwrap(),
            "right"
        );
    }

    #[test]
    fn deletion_is_synchronized() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("remove.txt"), "data");
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let first = service.compare(ScanMode::Verified).unwrap();
        service.synchronize(&first, &BTreeMap::new()).unwrap();
        fs::remove_file(source.join("remove.txt")).unwrap();
        let second = service.compare(ScanMode::Verified).unwrap();
        assert!(second.has_deletions());
        service.synchronize(&second, &BTreeMap::new()).unwrap();
        assert!(!briefcase.join("remove.txt").exists());
    }

    #[test]
    fn explicit_resolution_replaces_both_sides_and_sets_a_new_baseline() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("conflict.txt"), "base");
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let first = service.compare(ScanMode::Verified).unwrap();
        service.synchronize(&first, &BTreeMap::new()).unwrap();
        write(&source.join("conflict.txt"), "source wins");
        write(&briefcase.join("conflict.txt"), "portable loses");

        let plan = service.compare(ScanMode::Verified).unwrap();
        let mut resolutions = BTreeMap::new();
        resolutions.insert(
            PathBuf::from("conflict.txt"),
            ConflictResolution::KeepSource,
        );
        service.synchronize(&plan, &resolutions).unwrap();
        assert_eq!(
            fs::read_to_string(briefcase.join("conflict.txt")).unwrap(),
            "source wins"
        );
        let settled = service.compare(ScanMode::Verified).unwrap();
        assert_eq!(settled.actionable_count(), 0);
        assert!(settled.conflicts.is_empty());
    }

    #[test]
    fn deletion_conflict_can_accept_the_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        write(&source.join("choice.txt"), "base");
        let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
        let first = service.compare(ScanMode::Verified).unwrap();
        service.synchronize(&first, &BTreeMap::new()).unwrap();
        fs::remove_file(source.join("choice.txt")).unwrap();
        write(&briefcase.join("choice.txt"), "edited while away");

        let plan = service.compare(ScanMode::Verified).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        let mut resolutions = BTreeMap::new();
        resolutions.insert(
            PathBuf::from("choice.txt"),
            ConflictResolution::AcceptDeletion,
        );
        service.synchronize(&plan, &resolutions).unwrap();
        assert!(!briefcase.join("choice.txt").exists());
        assert!(service
            .compare(ScanMode::Verified)
            .unwrap()
            .conflicts
            .is_empty());
    }

    #[test]
    fn refuses_to_initialize_over_existing_content() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let briefcase = temp.path().join("portable");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&briefcase).unwrap();
        write(&briefcase.join("important.txt"), "do not overwrite");
        assert!(matches!(
            BriefcaseService::create(&source, &briefcase, "Bad"),
            Err(BriefcaseError::DestinationNotEmpty(_))
        ));
        assert_eq!(
            fs::read_to_string(briefcase.join("important.txt")).unwrap(),
            "do not overwrite"
        );
    }

    #[test]
    fn rejects_nested_roots_and_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(&source).unwrap();
        assert!(matches!(
            BriefcaseService::create(&source, &source.join("portable"), "Bad"),
            Err(BriefcaseError::OverlappingRoots)
        ));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/tmp", source.join("escape")).unwrap();
            let briefcase = temp.path().join("portable");
            let service = BriefcaseService::create(&source, &briefcase, "Test").unwrap();
            assert!(matches!(
                service.compare(ScanMode::Verified),
                Err(BriefcaseError::UnsupportedSymlink(_))
            ));
        }
    }
}
