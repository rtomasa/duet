use crate::{BriefcaseError, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub fn sha256_with_progress<F, C>(path: &Path, mut progress: F, is_cancelled: C) -> Result<String>
where
    F: FnMut(u64),
    C: Fn() -> bool,
{
    let file = File::open(path).map_err(|e| BriefcaseError::io(path, e))?;
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        if is_cancelled() {
            return Err(BriefcaseError::Cancelled);
        }
        let count = reader
            .read(&mut buffer)
            .map_err(|e| BriefcaseError::io(path, e))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
        progress(count as u64);
    }
    Ok(hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn hashing_can_be_cancelled_between_chunks() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("large.bin");
        std::fs::write(&path, vec![7_u8; 3 * 1024 * 1024]).unwrap();
        let cancelled = AtomicBool::new(false);

        let result = sha256_with_progress(
            &path,
            |_| cancelled.store(true, Ordering::Relaxed),
            || cancelled.load(Ordering::Relaxed),
        );

        assert!(matches!(result, Err(BriefcaseError::Cancelled)));
    }
}
