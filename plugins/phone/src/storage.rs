//! Local artifact storage helpers.

use std::path::{Path, PathBuf};

use crate::error::PhoneError;

pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis()
}

/// Write `bytes` to `dir/name` atomically (temp file in the same dir, then
/// `rename`), so a reader of `latest.jpg` never sees a torn frame.
pub async fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, PhoneError> {
    let final_path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    tokio::fs::write(&tmp, bytes)
        .await
        .map_err(|e| PhoneError::Backend(format!("write {}: {e}", tmp.display())))?;
    tokio::fs::rename(&tmp, &final_path)
        .await
        .map_err(|e| PhoneError::Backend(format!("rename to {}: {e}", final_path.display())))?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_write_replaces_and_leaves_no_temp() {
        let t = tempfile::tempdir().unwrap();
        write_atomic(t.path(), "latest.jpg", b"one").await.unwrap();
        let p = write_atomic(t.path(), "latest.jpg", b"two-two").await.unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two-two");
        let names: Vec<_> = std::fs::read_dir(t.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["latest.jpg".to_string()], "temp file left behind: {names:?}");
    }
}
