use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BriefcaseError {
    #[error("Could not access {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("The folder is not a valid Briefcase: {0}")]
    InvalidBriefcase(PathBuf),
    #[error("The destination folder already exists and is not empty: {0}")]
    DestinationNotEmpty(PathBuf),
    #[error("The Source and Briefcase folders cannot contain one another")]
    OverlappingRoots,
    #[error("The path “{0}” does not remain inside the synchronized folder")]
    UnsafePath(PathBuf),
    #[error("Symbolic links are not supported yet: {0}")]
    UnsupportedSymlink(PathBuf),
    #[error("Another synchronization is modifying this Briefcase")]
    AlreadyLocked,
    #[error("Synchronization was stopped")]
    Cancelled,
    #[error("The Source is unavailable: {0}")]
    SourceUnavailable(PathBuf),
    #[error("Unresolved conflict: {0}")]
    UnresolvedConflict(PathBuf),
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Invalid Briefcase metadata: {0}")]
    Manifest(#[from] serde_json::Error),
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl BriefcaseError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, BriefcaseError>;
