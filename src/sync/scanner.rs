use crate::{DuetError, EntryKind, FileSnapshot, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub fn scan_with_progress_and_cancel<F, C>(
    root: &Path,
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
        .filter_entry(|entry| entry.path() == root || entry.file_name() != ".duet");

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
        if file_type.is_symlink() {
            return Err(DuetError::UnsupportedSymlink(relative.to_path_buf()));
        }
        let kind = if file_type.is_dir() {
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
        let size = if kind == EntryKind::File {
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
            |completed, total| {
                if completed > 0 && total == 0 {
                    cancelled.store(true, Ordering::Relaxed);
                }
            },
            || cancelled.load(Ordering::Relaxed),
        );

        assert!(matches!(result, Err(DuetError::Cancelled)));
    }
}
