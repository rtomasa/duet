use super::hashing;
use crate::{BaselineEntry, BriefcaseError, EntryKind, FileSnapshot, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Fast,
    Verified,
}

#[derive(Debug, Clone, Copy)]
pub enum ScanSide {
    Source,
    Briefcase,
}

pub fn scan(
    root: &Path,
    side: ScanSide,
    baseline: &BTreeMap<PathBuf, BaselineEntry>,
    mode: ScanMode,
) -> Result<BTreeMap<PathBuf, FileSnapshot>> {
    if !root.is_dir() {
        return Err(BriefcaseError::SourceUnavailable(root.to_path_buf()));
    }
    let mut snapshots = BTreeMap::new();
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| entry.path() == root || entry.file_name() != ".briefcase");

    for item in walker {
        let item = item.map_err(|e| {
            let path = e.path().unwrap_or(root).to_path_buf();
            BriefcaseError::io(path, std::io::Error::new(std::io::ErrorKind::Other, e))
        })?;
        let path = item.path();
        if path == root {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| BriefcaseError::UnsafePath(path.into()))?;
        validate_relative(relative)?;
        let file_type = item.file_type();
        if file_type.is_symlink() {
            return Err(BriefcaseError::UnsupportedSymlink(relative.to_path_buf()));
        }
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };
        let metadata = fs::symlink_metadata(path).map_err(|e| BriefcaseError::io(path, e))?;
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
            Some(hashing::sha256(path)?)
        };
        snapshots.insert(
            relative.to_path_buf(),
            FileSnapshot {
                relative_path: relative.to_path_buf(),
                kind,
                size,
                mtime_ns,
                hash,
            },
        );
    }
    Ok(snapshots)
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
        ScanSide::Briefcase => {
            entry.briefcase_size == Some(size) && entry.briefcase_mtime_ns == Some(mtime_ns)
        }
    }
}

pub fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err(BriefcaseError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}
