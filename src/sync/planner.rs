use crate::{
    BaselineEntry, ChangeState, Conflict, FileSnapshot, PlannedOperation, SyncAction, SyncPlan,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

pub fn build_plan(
    baseline: &BTreeMap<PathBuf, BaselineEntry>,
    source: BTreeMap<PathBuf, FileSnapshot>,
    briefcase: BTreeMap<PathBuf, FileSnapshot>,
) -> SyncPlan {
    let paths: BTreeSet<_> = baseline
        .keys()
        .chain(source.keys())
        .chain(briefcase.keys())
        .cloned()
        .collect();
    let mut plan = SyncPlan {
        source_snapshots: source,
        briefcase_snapshots: briefcase,
        ..Default::default()
    };
    for path in paths {
        let previous = baseline.get(&path);
        let source_now = plan.source_snapshots.get(&path);
        let briefcase_now = plan.briefcase_snapshots.get(&path);
        let source_state = change_state(previous, source_now);
        let briefcase_state = change_state(previous, briefcase_now);
        let equal_now = source_now.is_some()
            && briefcase_now.is_some()
            && source_now.map(|s| (&s.kind, &s.hash)) == briefcase_now.map(|s| (&s.kind, &s.hash));
        let action = decide(source_state, briefcase_state, equal_now);
        let kind = source_now
            .map(|s| s.kind)
            .or_else(|| briefcase_now.map(|s| s.kind))
            .or_else(|| previous.map(|s| s.kind))
            .expect("path came from one of the maps");
        let operation = PlannedOperation {
            relative_path: path,
            kind,
            source_state,
            briefcase_state,
            action,
        };
        if action == SyncAction::Conflict {
            plan.conflicts.push(Conflict {
                source: source_now.cloned(),
                briefcase: briefcase_now.cloned(),
                operation,
            });
        } else {
            plan.operations.push(operation);
        }
    }
    plan
}

fn change_state(previous: Option<&BaselineEntry>, current: Option<&FileSnapshot>) -> ChangeState {
    match (previous, current) {
        (None, None) => ChangeState::Missing,
        (None, Some(_)) => ChangeState::Created,
        (Some(_), None) => ChangeState::Deleted,
        (Some(previous), Some(current)) => {
            if previous.kind == current.kind && previous.baseline_hash == current.hash {
                ChangeState::Unchanged
            } else {
                ChangeState::Modified
            }
        }
    }
}

pub fn decide(source: ChangeState, briefcase: ChangeState, equal_now: bool) -> SyncAction {
    use ChangeState::*;
    if equal_now
        && matches!(
            (source, briefcase),
            (Modified, Modified) | (Created, Created)
        )
    {
        return SyncAction::Adopt;
    }
    match (source, briefcase) {
        (Unchanged, Unchanged) => SyncAction::None,
        (Modified, Unchanged) => SyncAction::SourceToBriefcase,
        (Unchanged, Modified) => SyncAction::BriefcaseToSource,
        (Modified, Modified) => SyncAction::Conflict,
        (Deleted, Unchanged) => SyncAction::DeleteBriefcase,
        (Unchanged, Deleted) => SyncAction::DeleteSource,
        (Deleted, Modified) | (Modified, Deleted) => SyncAction::Conflict,
        (Deleted, Deleted) => SyncAction::RemoveBaseline,
        (Created, Missing) => SyncAction::SourceToBriefcase,
        (Missing, Created) => SyncAction::BriefcaseToSource,
        (Created, Created) => SyncAction::Conflict,
        _ => SyncAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implements_the_decision_matrix() {
        use ChangeState::*;
        use SyncAction::*;
        let cases = [
            (Unchanged, Unchanged, false, None),
            (Modified, Unchanged, false, SourceToBriefcase),
            (Unchanged, Modified, false, BriefcaseToSource),
            (Modified, Modified, false, Conflict),
            (Modified, Modified, true, Adopt),
            (Deleted, Unchanged, false, DeleteBriefcase),
            (Unchanged, Deleted, false, DeleteSource),
            (Deleted, Modified, false, Conflict),
            (Modified, Deleted, false, Conflict),
            (Deleted, Deleted, false, RemoveBaseline),
            (Created, Missing, false, SourceToBriefcase),
            (Missing, Created, false, BriefcaseToSource),
            (Created, Created, true, Adopt),
            (Created, Created, false, Conflict),
        ];
        for (source, briefcase, equal, expected) in cases {
            assert_eq!(decide(source, briefcase, equal), expected);
        }
    }
}
