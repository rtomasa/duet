use crate::{
    BaselineEntry, ChangeState, Conflict, FileSnapshot, PlannedOperation, SyncAction, SyncPlan,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub fn build_plan(
    baseline: &BTreeMap<PathBuf, BaselineEntry>,
    source: BTreeMap<PathBuf, FileSnapshot>,
    duet: BTreeMap<PathBuf, FileSnapshot>,
) -> SyncPlan {
    let paths: BTreeSet<_> = baseline
        .keys()
        .chain(source.keys())
        .chain(duet.keys())
        .cloned()
        .collect();
    let mut plan = SyncPlan {
        source_snapshots: source,
        duet_snapshots: duet,
        ..Default::default()
    };
    for path in paths {
        let previous = baseline.get(&path);
        let source_now = plan.source_snapshots.get(&path);
        let duet_now = plan.duet_snapshots.get(&path);
        let source_state = change_state(previous, source_now, BaselineSide::Source);
        let duet_state = change_state(previous, duet_now, BaselineSide::Target);
        let mut action = decide(source_state, duet_state);
        // A missing or lost state database leaves matching copies looking as
        // though they were independently created. Do not turn those into a
        // conflict (or overwrite either one): record their current metadata
        // as a fresh baseline instead.
        if previous.is_none()
            && action == SyncAction::Conflict
            && snapshots_match(source_now, duet_now)
        {
            action = SyncAction::RecordBaseline;
        }
        let kind = source_now
            .map(|s| s.kind)
            .or_else(|| duet_now.map(|s| s.kind))
            .or_else(|| previous.map(|s| s.kind))
            .expect("path came from one of the maps");
        let operation = PlannedOperation {
            relative_path: path,
            kind,
            source_state,
            duet_state,
            action,
        };
        if action == SyncAction::Conflict {
            plan.conflicts.push(Conflict {
                source: source_now.cloned(),
                duet: duet_now.cloned(),
                operation,
            });
        } else {
            plan.operations.push(operation);
        }
    }
    plan
}

fn snapshots_match(source: Option<&FileSnapshot>, duet: Option<&FileSnapshot>) -> bool {
    match (source, duet) {
        (Some(source), Some(duet)) if source.kind == crate::EntryKind::Directory => {
            duet.kind == crate::EntryKind::Directory
        }
        (Some(source), Some(duet)) => {
            source.kind == duet.kind && source.size == duet.size && source.mtime_ns == duet.mtime_ns
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum BaselineSide {
    Source,
    Target,
}

fn change_state(
    previous: Option<&BaselineEntry>,
    current: Option<&FileSnapshot>,
    side: BaselineSide,
) -> ChangeState {
    match (previous, current) {
        (None, None) => ChangeState::Missing,
        (None, Some(_)) => ChangeState::Created,
        (Some(_), None) => ChangeState::Deleted,
        (Some(previous), Some(current)) => {
            let (size, mtime_ns) = match side {
                BaselineSide::Source => (previous.source_size, previous.source_mtime_ns),
                BaselineSide::Target => (previous.duet_size, previous.duet_mtime_ns),
            };
            if previous.kind == current.kind
                && (current.kind == crate::EntryKind::Directory
                    || (size == Some(current.size) && mtime_ns == Some(current.mtime_ns)))
            {
                ChangeState::Unchanged
            } else {
                ChangeState::Modified
            }
        }
    }
}

pub fn decide(source: ChangeState, duet: ChangeState) -> SyncAction {
    use ChangeState::*;
    match (source, duet) {
        (Unchanged, Unchanged) => SyncAction::None,
        (Modified, Unchanged) => SyncAction::SourceToDuet,
        (Unchanged, Modified) => SyncAction::DuetToSource,
        (Modified, Modified) => SyncAction::Conflict,
        (Deleted, Unchanged) => SyncAction::DeleteDuet,
        (Unchanged, Deleted) => SyncAction::DeleteSource,
        (Deleted, Modified) | (Modified, Deleted) => SyncAction::Conflict,
        (Deleted, Deleted) => SyncAction::RemoveBaseline,
        (Created, Missing) => SyncAction::SourceToDuet,
        (Missing, Created) => SyncAction::DuetToSource,
        (Created, Created) => SyncAction::Conflict,
        _ => SyncAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_each_file_against_its_side_specific_metadata() {
        let previous = BaselineEntry {
            relative_path: PathBuf::from("document.txt"),
            kind: crate::EntryKind::File,
            source_size: Some(10),
            source_mtime_ns: Some(100),
            duet_size: Some(10),
            duet_mtime_ns: Some(200),
        };
        let source = FileSnapshot {
            relative_path: PathBuf::from("document.txt"),
            kind: crate::EntryKind::File,
            size: 10,
            mtime_ns: 100,
        };
        let target = FileSnapshot {
            relative_path: PathBuf::from("document.txt"),
            kind: crate::EntryKind::File,
            size: 10,
            mtime_ns: 201,
        };

        assert_eq!(
            change_state(Some(&previous), Some(&source), BaselineSide::Source),
            ChangeState::Unchanged
        );
        assert_eq!(
            change_state(Some(&previous), Some(&target), BaselineSide::Target),
            ChangeState::Modified
        );
    }

    #[test]
    fn implements_the_decision_matrix() {
        use ChangeState::*;
        use SyncAction::*;
        let cases = [
            (Unchanged, Unchanged, None),
            (Modified, Unchanged, SourceToDuet),
            (Unchanged, Modified, DuetToSource),
            (Modified, Modified, Conflict),
            (Deleted, Unchanged, DeleteDuet),
            (Unchanged, Deleted, DeleteSource),
            (Deleted, Modified, Conflict),
            (Modified, Deleted, Conflict),
            (Deleted, Deleted, RemoveBaseline),
            (Created, Missing, SourceToDuet),
            (Missing, Created, DuetToSource),
            (Created, Created, Conflict),
        ];
        for (source, duet, expected) in cases {
            assert_eq!(decide(source, duet), expected);
        }
    }

    #[test]
    fn matching_untracked_files_are_recorded_instead_of_conflicting() {
        let snapshot = FileSnapshot {
            relative_path: PathBuf::from("already-copied.bin"),
            kind: crate::EntryKind::File,
            size: 42,
            mtime_ns: 123,
        };
        let plan = build_plan(
            &BTreeMap::new(),
            BTreeMap::from([(snapshot.relative_path.clone(), snapshot.clone())]),
            BTreeMap::from([(snapshot.relative_path.clone(), snapshot)]),
        );

        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.operations.len(), 1);
        assert_eq!(plan.operations[0].action, SyncAction::RecordBaseline);
    }
}
