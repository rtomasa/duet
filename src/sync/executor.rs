use crate::{DuetError, EntryKind, FileSnapshot, PlannedOperation, Result, SyncAction};
use filetime::FileTime;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct MutationLock {
    file: File,
}

#[derive(Debug, Clone)]
pub struct CopiedMetadata {
    pub from_size: u64,
    pub from_mtime_ns: i64,
    pub to_size: u64,
    pub to_mtime_ns: i64,
}

impl MutationLock {
    pub fn acquire(duet_root: &Path) -> Result<Self> {
        let path = duet_root.join(".duet/lock");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| DuetError::io(&path, e))?;
        file.try_lock_exclusive()
            .map_err(|_| DuetError::AlreadyLocked)?;
        file.set_len(0).map_err(|e| DuetError::io(&path, e))?;
        writeln!(file, "pid={}", std::process::id()).map_err(|e| DuetError::io(&path, e))?;
        writeln!(file, "started_at={}", chrono::Utc::now().to_rfc3339())
            .map_err(|e| DuetError::io(&path, e))?;
        Ok(Self { file })
    }
}

impl Drop for MutationLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub fn ordered(operations: &[PlannedOperation]) -> Vec<PlannedOperation> {
    let mut result = operations.to_vec();
    result.sort_by_key(order_key);
    result
}

fn order_key(op: &PlannedOperation) -> (u8, usize, PathBuf) {
    let depth = op.relative_path.components().count();
    let phase = match (op.action, op.kind) {
        (SyncAction::SourceToDuet | SyncAction::DuetToSource, EntryKind::Directory) => 0,
        (SyncAction::SourceToDuet | SyncAction::DuetToSource, EntryKind::File) => 1,
        (SyncAction::DeleteSource | SyncAction::DeleteDuet, EntryKind::File) => 2,
        (SyncAction::DeleteSource | SyncAction::DeleteDuet, EntryKind::Directory) => 3,
        _ => 4,
    };
    let depth_order = if phase == 3 {
        usize::MAX - depth
    } else {
        depth
    };
    (phase, depth_order, op.relative_path.clone())
}

pub fn apply_one_with_progress(
    source_root: &Path,
    duet_root: &Path,
    op: &PlannedOperation,
    expected: Option<&FileSnapshot>,
    progress: &mut dyn FnMut(u64, u64),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Option<CopiedMetadata>> {
    check_cancelled(is_cancelled)?;
    match op.action {
        SyncAction::SourceToDuet => copy_entry(
            source_root,
            duet_root,
            &op.relative_path,
            op.kind,
            expected,
            progress,
            is_cancelled,
        ),
        SyncAction::DuetToSource => copy_entry(
            duet_root,
            source_root,
            &op.relative_path,
            op.kind,
            expected,
            progress,
            is_cancelled,
        ),
        SyncAction::DeleteSource => {
            let result = delete_entry(source_root, &op.relative_path, op.kind);
            if result.is_ok() {
                progress(1, 1);
            }
            result.map(|()| None)
        }
        SyncAction::DeleteDuet => {
            let result = delete_entry(duet_root, &op.relative_path, op.kind);
            if result.is_ok() {
                progress(1, 1);
            }
            result.map(|()| None)
        }
        SyncAction::None | SyncAction::RemoveBaseline => {
            progress(1, 1);
            Ok(None)
        }
        SyncAction::Conflict => Err(DuetError::UnresolvedConflict(op.relative_path.clone())),
    }
}

fn copy_entry(
    from_root: &Path,
    to_root: &Path,
    relative: &Path,
    kind: EntryKind,
    expected: Option<&FileSnapshot>,
    progress: &mut dyn FnMut(u64, u64),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Option<CopiedMetadata>> {
    let from = checked_existing(from_root, relative)?;
    let to = checked_destination(to_root, relative)?;
    if kind == EntryKind::Directory {
        fs::create_dir_all(&to).map_err(|e| DuetError::io(&to, e))?;
        checked_parent(to_root, &to)?;
        progress(1, 1);
        let from_metadata = fs::metadata(&from).map_err(|e| DuetError::io(&from, e))?;
        let to_metadata = fs::metadata(&to).map_err(|e| DuetError::io(&to, e))?;
        return Ok(Some(CopiedMetadata {
            from_size: 0,
            from_mtime_ns: modified_ns(&from_metadata),
            to_size: 0,
            to_mtime_ns: modified_ns(&to_metadata),
        }));
    }
    if fs::symlink_metadata(&from)
        .map_err(|e| DuetError::io(&from, e))?
        .file_type()
        .is_symlink()
    {
        return Err(DuetError::UnsupportedSymlink(relative.to_path_buf()));
    }
    let parent = to
        .parent()
        .ok_or_else(|| DuetError::UnsafePath(to.clone()))?;
    fs::create_dir_all(parent).map_err(|e| DuetError::io(parent, e))?;
    checked_parent(to_root, &to)?;

    let mut input = File::open(&from).map_err(|e| DuetError::io(&from, e))?;
    let metadata = input.metadata().map_err(|e| DuetError::io(&from, e))?;
    ensure_matches_checked_snapshot(relative, &metadata, expected)?;
    let total = metadata.len();
    let progress_total = total.max(1);
    progress(0, progress_total);
    let mut temporary = tempfile::Builder::new()
        .prefix(".duet-tmp-")
        .tempfile_in(parent)
        .map_err(|e| DuetError::io(parent, e))?;
    let mut buffer = [0_u8; 1024 * 1024];
    let mut written = 0_u64;
    loop {
        check_cancelled(is_cancelled)?;
        let count = input
            .read(&mut buffer)
            .map_err(|e| DuetError::io(&from, e))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|e| DuetError::io(&to, e))?;
        written += count as u64;
        // Reserve 100% for the point at which the complete temporary file has
        // been flushed, persisted, and had its metadata applied.
        if written < total {
            progress(written, progress_total);
        }
    }
    check_cancelled(is_cancelled)?;
    temporary.flush().map_err(|e| DuetError::io(&to, e))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| DuetError::io(&to, e))?;
    if written != metadata.len() {
        return Err(DuetError::Other(anyhow::anyhow!(
            "The copied size of {} does not match the original",
            relative.display()
        )));
    }
    temporary
        .persist(&to)
        .map_err(|e| DuetError::io(&to, e.error))?;
    if let Ok(modified) = metadata.modified() {
        let time = FileTime::from_system_time(modified);
        filetime::set_file_mtime(&to, time).map_err(|e| DuetError::io(&to, e))?;
    }
    let from_metadata = fs::metadata(&from).map_err(|e| DuetError::io(&from, e))?;
    ensure_matches_checked_snapshot(relative, &from_metadata, expected)?;
    let to_metadata = fs::metadata(&to).map_err(|e| DuetError::io(&to, e))?;
    if to_metadata.len() != written {
        return Err(DuetError::Other(anyhow::anyhow!(
            "The copied size of {} does not match the original",
            relative.display()
        )));
    }
    progress(progress_total, progress_total);
    Ok(Some(CopiedMetadata {
        from_size: from_metadata.len(),
        from_mtime_ns: modified_ns(&from_metadata),
        to_size: to_metadata.len(),
        to_mtime_ns: modified_ns(&to_metadata),
    }))
}

fn ensure_matches_checked_snapshot(
    relative: &Path,
    metadata: &fs::Metadata,
    expected: Option<&FileSnapshot>,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    if expected.kind != EntryKind::File
        || expected.size != metadata.len()
        || expected.mtime_ns != modified_ns(metadata)
    {
        return Err(changed_after_check(relative));
    }
    Ok(())
}

fn changed_after_check(relative: &Path) -> DuetError {
    DuetError::Other(anyhow::anyhow!(
        "{} changed after the folders were checked; check again before synchronizing",
        relative.display()
    ))
}

fn modified_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
        })
        .unwrap_or(0)
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
    if is_cancelled() {
        Err(DuetError::Cancelled)
    } else {
        Ok(())
    }
}

fn delete_entry(root: &Path, relative: &Path, kind: EntryKind) -> Result<()> {
    let path = lexical_join(root, relative)?;
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        return Err(DuetError::UnsupportedSymlink(relative.to_path_buf()));
    }
    let actual = path.canonicalize().map_err(|e| DuetError::io(&path, e))?;
    let root = root.canonicalize().map_err(|e| DuetError::io(root, e))?;
    if !actual.starts_with(&root) || actual == root {
        return Err(DuetError::UnsafePath(relative.to_path_buf()));
    }
    if kind == EntryKind::Directory {
        fs::remove_dir(&actual).map_err(|e| DuetError::io(&actual, e))
    } else {
        fs::remove_file(&actual).map_err(|e| DuetError::io(&actual, e))
    }
}

fn lexical_join(root: &Path, relative: &Path) -> Result<PathBuf> {
    super::scanner::validate_relative(relative)?;
    Ok(root.join(relative))
}

fn checked_existing(root: &Path, relative: &Path) -> Result<PathBuf> {
    let path = lexical_join(root, relative)?;
    let root_actual = root.canonicalize().map_err(|e| DuetError::io(root, e))?;
    let actual = path.canonicalize().map_err(|e| DuetError::io(&path, e))?;
    if !actual.starts_with(&root_actual) || actual == root_actual {
        return Err(DuetError::UnsafePath(relative.to_path_buf()));
    }
    Ok(actual)
}

fn checked_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
    lexical_join(root, relative)
}

fn checked_parent(root: &Path, destination: &Path) -> Result<()> {
    let root_actual = root.canonicalize().map_err(|e| DuetError::io(root, e))?;
    let parent = destination
        .parent()
        .ok_or_else(|| DuetError::UnsafePath(destination.into()))?;
    let parent_actual = parent
        .canonicalize()
        .map_err(|e| DuetError::io(parent, e))?;
    if !parent_actual.starts_with(&root_actual) {
        return Err(DuetError::UnsafePath(destination.into()));
    }
    Ok(())
}
