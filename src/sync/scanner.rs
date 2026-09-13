use crate::{DuetError, EntryKind, FileSnapshot, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub fn scan_with_progress_and_cancel<F, C>(
    root: &Path,
    skip_hidden_files: bool,
    copy_symbolic_links: bool,
    progress: F,
    is_cancelled: C,
) -> Result<BTreeMap<PathBuf, FileSnapshot>>
where
    F: Fn(u64, u64) + Sync,
    C: Fn() -> bool + Sync,
{
    if !root.is_dir() {
        return Err(DuetError::SourceUnavailable(root.to_path_buf()));
    }
    let mut snapshots = Vec::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.path() == root
                || (entry.file_name() != ".duet"
                    && (!skip_hidden_files
                        || entry
                            .path()
                            .strip_prefix(root)
                            .map(|path| !is_hidden_path(path))
                            .unwrap_or(true)))
        });

    for item in walker {
        check_cancelled(&is_cancelled)?;
        let item = item.map_err(|e| {
            let path = e.path().unwrap_or(root).to_path_buf();
            DuetError::io(path, std::io::Error::other(e))
        })?;
        let path = item.path();
        if path == root {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| DuetError::UnsafePath(path.into()))?;
        validate_relative(relative)?;
        let metadata = fs::symlink_metadata(path).map_err(|e| DuetError::io(path, e))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() && !copy_symbolic_links {
            continue;
        }
        let kind = if file_type.is_symlink() {
            EntryKind::SymbolicLink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let mtime_ns = metadata
            .modified()
            .ok()
            .and_then(|time| {
                time.duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|duration| i64::try_from(duration.as_nanos()).ok())
            })
            .unwrap_or(0);
        let size = if matches!(kind, EntryKind::File | EntryKind::SymbolicLink) {
            metadata.len()
        } else {
            0
        };
        snapshots.push(FileSnapshot {
            relative_path: relative.to_path_buf(),
            kind,
            size,
            mtime_ns,
        });
        progress(snapshots.len() as u64, 0);
    }

    Ok(snapshots
        .into_iter()
        .map(|snapshot| (snapshot.relative_path.clone(), snapshot))
        .collect())
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

pub fn is_hidden_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        std::path::Component::Normal(name) => is_hidden(name),
        _ => false,
    })
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
    if is_cancelled() {
        Err(DuetError::Cancelled)
    } else {
        Ok(())
    }
}

pub fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(DuetError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn scan_reports_discovery_progress() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("one.bin"), vec![1_u8; 1024]).unwrap();
        fs::write(temp.path().join("two.bin"), vec![2_u8; 2048]).unwrap();
        let events = std::sync::Mutex::new(Vec::new());

        let snapshots = scan_with_progress_and_cancel(
            temp.path(),
            false,
            true,
            |completed, total| events.lock().unwrap().push((completed, total)),
            || false,
        )
        .unwrap();

        assert_eq!(snapshots.len(), 2);
        let events = events.into_inner().unwrap();
        assert!(events
            .iter()
            .any(|(completed, total)| *completed == 2 && *total == 0));
    }

    #[test]
    fn scan_can_be_cancelled_while_discovering_entries() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("one.bin"), b"one").unwrap();
        fs::write(temp.path().join("two.bin"), b"two").unwrap();
        let cancelled = AtomicBool::new(false);

        let result = scan_with_progress_and_cancel(
            temp.path(),
            false,
            true,
            |completed, total| {
                if completed > 0 && total == 0 {
                    cancelled.store(true, Ordering::Relaxed);
                }
            },
            || cancelled.load(Ordering::Relaxed),
        );

        assert!(matches!(result, Err(DuetError::Cancelled)));
    }

    #[test]
    fn scan_can_skip_hidden_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("visible.txt"), b"visible").unwrap();
        fs::write(temp.path().join(".hidden.txt"), b"hidden").unwrap();
        fs::create_dir(temp.path().join(".hidden")).unwrap();
        fs::write(temp.path().join(".hidden/nested.txt"), b"hidden").unwrap();

        let snapshots =
            scan_with_progress_and_cancel(temp.path(), true, true, |_, _| {}, || false).unwrap();

        assert_eq!(snapshots.len(), 1);
        assert!(snapshots.contains_key(Path::new("visible.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn scan_copies_or_skips_symbolic_links() {
        let temp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("missing-target", temp.path().join("link")).unwrap();

        let copied =
            scan_with_progress_and_cancel(temp.path(), false, true, |_, _| {}, || false).unwrap();
        assert_eq!(copied[Path::new("link")].kind, EntryKind::SymbolicLink);

        let skipped =
            scan_with_progress_and_cancel(temp.path(), false, false, |_, _| {}, || false).unwrap();
        assert!(!skipped.contains_key(Path::new("link")));
    }
}
