use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum BriefcaseError {
    #[error("No se pudo acceder a {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("La carpeta no es un maletín válido: {0}")]
    InvalidBriefcase(PathBuf),
    #[error("La carpeta de destino ya existe y no está vacía: {0}")]
    DestinationNotEmpty(PathBuf),
    #[error("La carpeta Source y el Briefcase no pueden contenerse una dentro de otra")]
    OverlappingRoots,
    #[error("La ruta «{0}» no permanece dentro de la carpeta sincronizada")]
    UnsafePath(PathBuf),
    #[error("Los enlaces simbólicos aún no son compatibles: {0}")]
    UnsupportedSymlink(PathBuf),
    #[error("Hay otra sincronización modificando este maletín")]
    AlreadyLocked,
    #[error("El origen no está disponible: {0}")]
    SourceUnavailable(PathBuf),
    #[error("Conflicto sin resolver: {0}")]
    UnresolvedConflict(PathBuf),
    #[error("Error en la base de datos: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Metadatos del maletín no válidos: {0}")]
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
