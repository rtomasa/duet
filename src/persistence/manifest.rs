use crate::{DuetError, DuetManifest, Result, SourceLocator};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct ManifestRepository;

impl ManifestRepository {
    pub fn create(
        duet_root: &Path,
        source_root: &Path,
        name: impl Into<String>,
    ) -> Result<DuetManifest> {
        fs::create_dir_all(duet_root.join(".duet/transactions"))
            .map_err(|e| DuetError::io(duet_root, e))?;
        let manifest = DuetManifest {
            format_version: 1,
            duet_id: uuid::Uuid::now_v7(),
            name: name.into(),
            created_at: chrono::Utc::now(),
            source: SourceLocator {
                last_known_path: source_root.to_path_buf(),
            },
        };
        Self::save(duet_root, &manifest)?;
        Ok(manifest)
    }

    pub fn load(duet_root: &Path) -> Result<DuetManifest> {
        let path = Self::path(duet_root);
        if !path.is_file() {
            return Err(DuetError::InvalidDuet(duet_root.to_path_buf()));
        }
        let bytes = fs::read(&path).map_err(|e| DuetError::io(&path, e))?;
        let manifest: DuetManifest = serde_json::from_slice(&bytes)?;
        if manifest.format_version != 1 {
            return Err(DuetError::InvalidDuet(duet_root.to_path_buf()));
        }
        Ok(manifest)
    }

    pub fn rebind_source(duet_root: &Path, source_root: PathBuf) -> Result<DuetManifest> {
        let mut manifest = Self::load(duet_root)?;
        manifest.source.last_known_path = source_root;
        Self::save(duet_root, &manifest)?;
        Ok(manifest)
    }

    pub fn save(duet_root: &Path, manifest: &DuetManifest) -> Result<()> {
        let path = Self::path(duet_root);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        let parent = path
            .parent()
            .ok_or_else(|| DuetError::InvalidDuet(duet_root.to_path_buf()))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".manifest-")
            .tempfile_in(parent)
            .map_err(|e| DuetError::io(parent, e))?;
        temporary
            .write_all(&bytes)
            .map_err(|e| DuetError::io(&path, e))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| DuetError::io(&path, e))?;
        temporary
            .persist(&path)
            .map_err(|e| DuetError::io(&path, e.error))?;
        Ok(())
    }

    pub fn path(duet_root: &Path) -> PathBuf {
        duet_root.join(".duet/manifest.json")
    }
}
