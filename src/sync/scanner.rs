use super::hashing;
use crate::{BaselineEntry, DuetError, EntryKind, FileSnapshot, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

const MAX_HASH_WORKERS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Fast,
    Verified,
}

#[derive(Debug, Clone, Copy)]
pub enum ScanSide {
    Source,
    Duet,
}

pub fn scan_with_progress_and_cancel<F, C>(
    root: &Path,
    side: ScanSide,
    baseline: &BTreeMap<PathBuf, BaselineEntry>,
    mode: ScanMode,
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
    let mut hash_tasks = Vec::new();
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
        let hash = if kind == EntryKind::Directory {
            None
        } else if mode == ScanMode::Fast
            && metadata_matches(baseline.get(relative), side, size, mtime_ns)
        {
            baseline
                .get(relative)
                .and_then(|entry| entry.baseline_hash.clone())
        } else {
            None
        };
        let snapshot_index = snapshots.len();
        snapshots.push(FileSnapshot {
            relative_path: relative.to_path_buf(),
            kind,
            size,
            mtime_ns,
            hash,
        });
        if kind == EntryKind::File && snapshots[snapshot_index].hash.is_none() {
            hash_tasks.push((snapshot_index, path.to_path_buf(), size.max(1)));
        }
        progress(snapshots.len() as u64, 0);
    }

    hash_files(&mut snapshots, &hash_tasks, &progress, &is_cancelled)?;
    Ok(snapshots
        .into_iter()
        .map(|snapshot| (snapshot.relative_path.clone(), snapshot))
        .collect())
}

fn hash_files<F, C>(
    snapshots: &mut [FileSnapshot],
    tasks: &[(usize, PathBuf, u64)],
    progress: &F,
    is_cancelled: &C,
) -> Result<()>
where
    F: Fn(u64, u64) + Sync,
    C: Fn() -> bool + Sync,
{
    if tasks.is_empty() {
        return Ok(());
    }
    check_cancelled(is_cancelled)?;
    let total = tasks.iter().map(|(_, _, work)| work).sum();
    progress(0, total);

    let available = thread::available_parallelism().map_or(1, usize::from);
    let worker_count = available.min(MAX_HASH_WORKERS).min(tasks.len());
    let next = AtomicUsize::new(0);
    let completed = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let (sender, receiver) = mpsc::channel();
    let mut first_error = None;
    let mut was_cancelled = false;

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let sender = sender.clone();
            let next = &next;
            let completed = &completed;
            let stop = &stop;
            scope.spawn(move || loop {
                if stop.load(Ordering::Relaxed) || is_cancelled() {
                    break;
                }
                let task_index = next.fetch_add(1, Ordering::Relaxed);
                let Some((snapshot_index, path, work)) = tasks.get(task_index) else {
                    break;
                };
                let mut file_progress = 0_u64;
                let result = hashing::sha256_with_progress(
                    path,
                    |delta| {
                        file_progress += delta;
                        let done = completed.fetch_add(delta, Ordering::Relaxed) + delta;
                        progress(done.min(total), total);
                    },
                    || stop.load(Ordering::Relaxed) || is_cancelled(),
                );
                if result.is_ok() && file_progress < *work {
                    let remaining = *work - file_progress;
                    let done = completed.fetch_add(remaining, Ordering::Relaxed) + remaining;
                    progress(done.min(total), total);
                }
                if result.is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
                if sender.send((*snapshot_index, result)).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        for (snapshot_index, result) in receiver {
            match result {
                Ok(hash) => snapshots[snapshot_index].hash = Some(hash),
                Err(DuetError::Cancelled) => was_cancelled = true,
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
    });

    if let Some(error) = first_error {
        return Err(error);
    }
    if was_cancelled {
        return Err(DuetError::Cancelled);
    }
    check_cancelled(is_cancelled)
}

fn check_cancelled(is_cancelled: &dyn Fn() -> bool) -> Result<()> {
    if is_cancelled() {
        Err(DuetError::Cancelled)
    } else {
        Ok(())
    }
}

fn metadata_matches(
    entry: Option<&BaselineEntry>,
    side: ScanSide,
    size: u64,
    mtime_ns: i64,
) -> bool {
    let Some(entry) = entry else { return false };
    match side {
        ScanSide::Source => {
            entry.source_size == Some(size) && entry.source_mtime_ns == Some(mtime_ns)
        }
        ScanSide::Duet => entry.duet_size == Some(size) && entry.duet_mtime_ns == Some(mtime_ns),
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
    use std::sync::atomic::AtomicBool;

    #[test]
    fn scan_reports_discovery_and_hash_progress() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("one.bin"), vec![1_u8; 1024]).unwrap();
        fs::write(temp.path().join("two.bin"), vec![2_u8; 2048]).unwrap();
        let events = std::sync::Mutex::new(Vec::new());

        let snapshots = scan_with_progress_and_cancel(
            temp.path(),
            ScanSide::Source,
            &BTreeMap::new(),
            ScanMode::Verified,
            |completed, total| events.lock().unwrap().push((completed, total)),
            || false,
        )
        .unwrap();

        assert_eq!(snapshots.len(), 2);
        let events = events.into_inner().unwrap();
        assert!(events
            .iter()
            .any(|(completed, total)| *completed > 0 && *total == 0));
        assert!(events
            .iter()
            .any(|(completed, total)| *total > 0 && completed == total));
    }

    #[test]
    fn scan_can_be_cancelled_while_discovering_entries() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("one.bin"), b"one").unwrap();
        fs::write(temp.path().join("two.bin"), b"two").unwrap();
        let cancelled = AtomicBool::new(false);

        let result = scan_with_progress_and_cancel(
            temp.path(),
            ScanSide::Source,
            &BTreeMap::new(),
            ScanMode::Verified,
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
