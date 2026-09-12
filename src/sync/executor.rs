use crate::{BriefcaseError, EntryKind, PlannedOperation, Result, SyncAction};
use filetime::FileTime;
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub struct MutationLock {
    file: File,
}

impl MutationLock {
    pub fn acquire(briefcase_root: &Path) -> Result<Self> {
        let path = briefcase_root.join(".briefcase/lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| BriefcaseError::io(&path, e))?;
        file.try_lock_exclusive()
            .map_err(|_| BriefcaseError::AlreadyLocked)?;
        file.set_len(0).map_err(|e| BriefcaseError::io(&path, e))?;
        writeln!(file, "pid={}", std::process::id()).map_err(|e| BriefcaseError::io(&path, e))?;
        writeln!(file, "started_at={}", chrono::Utc::now().to_rfc3339())
            .map_err(|e| BriefcaseError::io(&path, e))?;
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
        (SyncAction::SourceToBriefcase | SyncAction::BriefcaseToSource, EntryKind::Directory) => 0,
        (SyncAction::SourceToBriefcase | SyncAction::BriefcaseToSource, EntryKind::File) => 1,
        (SyncAction::DeleteSource | SyncAction::DeleteBriefcase, EntryKind::File) => 2,
        (SyncAction::DeleteSource | SyncAction::DeleteBriefcase, EntryKind::Directory) => 3,
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
    briefcase_root: &Path,
    op: &PlannedOperation,
    progress: &mut dyn FnMut(u64, u64),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    check_cancelled(is_cancelled)?;
    match op.action {
        SyncAction::SourceToBriefcase => {
            copy_entry(
                source_root,
                briefcase_root,
                &op.relative_path,
                op.kind,
                progress,
                is_cancelled,
            )
        }
        SyncAction::BriefcaseToSource => {
            copy_entry(
                briefcase_root,
                source_root,
                &op.relative_path,
                op.kind,
                progress,
                is_cancelled,
            )
        }
        SyncAction::DeleteSource => {
            let result = delete_entry(source_root, &op.relative_path, op.kind);
            if result.is_ok() {
                progress(1, 1);
            }
            result
        }
        SyncAction::DeleteBriefcase => {
            let result = delete_entry(briefcase_root, &op.relative_path, op.kind);
            if result.is_ok() {
                progress(1, 1);
            }
            result
        }
        SyncAction::None | SyncAction::Adopt | SyncAction::RemoveBaseline => {
            progress(1, 1);
            Ok(())
        }
        SyncAction::Conflict => Err(BriefcaseError::UnresolvedConflict(op.relative_path.clone())),
    }
}

fn copy_entry(
    from_root: &Path,
    to_root: &Path,
    relative: &Path,
    kind: EntryKind,
    progress: &mut dyn FnMut(u64, u64),
    is_cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    let from = checked_existing(from_root, relative)?;
    let to = checked_destination(to_root, relative)?;
    if kind == EntryKind::Directory {
        fs::create_dir_all(&to).map_err(|e| BriefcaseError::io(&to, e))?;
        checked_parent(to_root, &to)?;
        progress(1, 1);
        return Ok(());
    }
    if fs::symlink_metadata(&from)
        .map_err(|e| BriefcaseError::io(&from, e))?
        .file_type()
        .is_symlink()
    {
        return Err(BriefcaseError::UnsupportedSymlink(relative.to_path_buf()));
    }
    let parent = to
        .parent()
        .ok_or_else(|| BriefcaseError::UnsafePath(to.clone()))?;
    fs::create_dir_all(parent).map_err(|e| BriefcaseError::io(parent, e))?;
    checked_parent(to_root, &to)?;

    let mut input = File::open(&from).map_err(|e| BriefcaseError::io(&from, e))?;
    let metadata = input.metadata().map_err(|e| BriefcaseError::io(&from, e))?;
    let total = metadata.len();
    progress(0, total);
    let mut temporary = tempfile::Builder::new()
        .prefix(".briefcase-tmp-")
        .tempfile_in(parent)
        .map_err(|e| BriefcaseError::io(parent, e))?;
    let mut buffer = [0_u8; 1024 * 1024];
    let mut written = 0_u64;
    loop {
        check_cancelled(is_cancelled)?;
        let count = input
            .read(&mut buffer)
            .map_err(|e| BriefcaseError::io(&from, e))?;
        if count == 0 {
            break;
        }
        temporary
            .write_all(&buffer[..count])
            .map_err(|e| BriefcaseError::io(&to, e))?;
        written += count as u64;
        progress(written, total);
    }
    check_cancelled(is_cancelled)?;
    temporary.flush().map_err(|e| BriefcaseError::io(&to, e))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|e| BriefcaseError::io(&to, e))?;
    if written != metadata.len() {
        return Err(BriefcaseError::Other(anyhow::anyhow!(
            "The copied size of {} does not match the original",
            relative.display()
        )));
    }
    temporary
        .persist(&to)
        .map_err(|e| BriefcaseError::io(&to, e.error))?;
    if let Ok(modified) = metadata.modified() {
        let time = FileTime::from_system_time(modified);
        filetime::set_file_mtime(&to, time).map_err(|e| BriefcaseError::io(&to, e))?;
    }
    Ok(())
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
    if is_cancelled() {
        Err(BriefcaseError::Cancelled)
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
        return Err(BriefcaseError::UnsupportedSymlink(relative.to_path_buf()));
    }
    let actual = path
        .canonicalize()
        .map_err(|e| BriefcaseError::io(&path, e))?;
    let root = root
        .canonicalize()
        .map_err(|e| BriefcaseError::io(root, e))?;
    if !actual.starts_with(&root) || actual == root {
        return Err(BriefcaseError::UnsafePath(relative.to_path_buf()));
    }
    if kind == EntryKind::Directory {
        fs::remove_dir(&actual).map_err(|e| BriefcaseError::io(&actual, e))
    } else {
        fs::remove_file(&actual).map_err(|e| BriefcaseError::io(&actual, e))
    }
}

fn lexical_join(root: &Path, relative: &Path) -> Result<PathBuf> {
    super::scanner::validate_relative(relative)?;
    Ok(root.join(relative))
}

fn checked_existing(root: &Path, relative: &Path) -> Result<PathBuf> {
    let path = lexical_join(root, relative)?;
    let root_actual = root
        .canonicalize()
        .map_err(|e| BriefcaseError::io(root, e))?;
    let actual = path
        .canonicalize()
        .map_err(|e| BriefcaseError::io(&path, e))?;
    if !actual.starts_with(&root_actual) || actual == root_actual {
        return Err(BriefcaseError::UnsafePath(relative.to_path_buf()));
    }
    Ok(actual)
}

fn checked_destination(root: &Path, relative: &Path) -> Result<PathBuf> {
    lexical_join(root, relative)
}

fn checked_parent(root: &Path, destination: &Path) -> Result<()> {
    let root_actual = root
        .canonicalize()
        .map_err(|e| BriefcaseError::io(root, e))?;
    let parent = destination
        .parent()
        .ok_or_else(|| BriefcaseError::UnsafePath(destination.into()))?;
    let parent_actual = parent
        .canonicalize()
        .map_err(|e| BriefcaseError::io(parent, e))?;
    if !parent_actual.starts_with(&root_actual) {
        return Err(BriefcaseError::UnsafePath(destination.into()));
    }
    Ok(())
}
