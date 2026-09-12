use crate::{BriefcaseError, BriefcaseManifest, Result, SourceLocator};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct ManifestRepository;

impl ManifestRepository {
    pub fn create(
        briefcase_root: &Path,
        source_root: &Path,
        name: impl Into<String>,
    ) -> Result<BriefcaseManifest> {
        fs::create_dir_all(briefcase_root.join(".briefcase/transactions"))
            .map_err(|e| BriefcaseError::io(briefcase_root, e))?;
        let manifest = BriefcaseManifest {
            format_version: 1,
            briefcase_id: uuid::Uuid::now_v7(),
            name: name.into(),
            created_at: chrono::Utc::now(),
            source: SourceLocator {
                last_known_path: source_root.to_path_buf(),
            },
        };
        Self::save(briefcase_root, &manifest)?;
        Ok(manifest)
    }

    pub fn load(briefcase_root: &Path) -> Result<BriefcaseManifest> {
        let path = Self::path(briefcase_root);
        if !path.is_file() {
            return Err(BriefcaseError::InvalidBriefcase(
                briefcase_root.to_path_buf(),
            ));
        }
        let bytes = fs::read(&path).map_err(|e| BriefcaseError::io(&path, e))?;
        let manifest: BriefcaseManifest = serde_json::from_slice(&bytes)?;
        if manifest.format_version != 1 {
            return Err(BriefcaseError::InvalidBriefcase(
                briefcase_root.to_path_buf(),
            ));
        }
        Ok(manifest)
    }

    pub fn rebind_source(briefcase_root: &Path, source_root: PathBuf) -> Result<BriefcaseManifest> {
        let mut manifest = Self::load(briefcase_root)?;
        manifest.source.last_known_path = source_root;
        Self::save(briefcase_root, &manifest)?;
        Ok(manifest)
    }

    pub fn save(briefcase_root: &Path, manifest: &BriefcaseManifest) -> Result<()> {
        let path = Self::path(briefcase_root);
        let bytes = serde_json::to_vec_pretty(manifest)?;
        let parent = path
            .parent()
            .ok_or_else(|| BriefcaseError::InvalidBriefcase(briefcase_root.to_path_buf()))?;
        let mut temporary = tempfile::Builder::new()
            .prefix(".manifest-")
            .tempfile_in(parent)
            .map_err(|e| BriefcaseError::io(parent, e))?;
        temporary
            .write_all(&bytes)
            .map_err(|e| BriefcaseError::io(&path, e))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| BriefcaseError::io(&path, e))?;
        temporary
            .persist(&path)
            .map_err(|e| BriefcaseError::io(&path, e.error))?;
        Ok(())
    }

    pub fn path(briefcase_root: &Path) -> PathBuf {
        briefcase_root.join(".briefcase/manifest.json")
    }
}
