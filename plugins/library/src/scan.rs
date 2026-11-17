use crate::store::{id_for_path, kind_for_ext, now_ms, Entry};
use std::fs;
use std::path::PathBuf;
use walkdir::WalkDir;

fn allowed_roots_from_env(override_roots: Option<Vec<String>>) -> Vec<PathBuf> {
    if let Some(roots) = override_roots {
        return roots
            .into_iter()
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .collect();
    }
    if let Ok(raw) = std::env::var("LIBRARY_PLUGIN_ALLOWED_ROOTS") {
        if raw.trim().is_empty() {
            return Vec::new();
        }
        return raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .collect();
    }
    Vec::new()
}

fn max_scan_files() -> usize {
    std::env::var("LIBRARY_PLUGIN_MAX_SCAN_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10000)
}

#[derive(Debug)]
pub struct ScanResult {
    pub scanned: usize,
    pub indexed: usize,
    pub removed: usize,
    pub entries: Vec<Entry>,
}

pub fn scan_filesystem(roots: Option<Vec<String>>, _force: bool) -> Result<ScanResult, String> {
    let roots = allowed_roots_from_env(roots);
    if roots.is_empty() {
        return Err("ERR_LIBRARY_NO_ROOTS: set LIBRARY_PLUGIN_ALLOWED_ROOTS to comma-separated absolute paths".into());
    }
    let max_files = max_scan_files();
    let mut scanned = 0usize;
    let mut entries = Vec::new();

    for root in roots {
        if !root.exists() {
            eprintln!(
                "[library] root does not exist, skipping: {}",
                root.display()
            );
            continue;
        }
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if scanned >= max_files {
                eprintln!("[library] hit max scan files {max_files}, truncating");
                break;
            }
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // skip hidden files and temp files
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_string();
            let kind = kind_for_ext(&ext);
            if kind == "other" {
                // still index other? No, skip to keep index focused
                continue;
            }
            let meta = match fs::metadata(path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let size = meta.len();
            let mtime_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let path_str = path.display().to_string();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let id = id_for_path(&path_str);
            entries.push(Entry {
                id,
                path: path_str,
                name,
                kind: kind.to_string(),
                ext: ext.to_ascii_lowercase(),
                size_bytes: size,
                mtime_ms,
                indexed_at_ms: now_ms(),
            });
            scanned += 1;
        }
    }

    Ok(ScanResult {
        scanned,
        indexed: 0,
        removed: 0,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn kind_detection() {
        assert_eq!(kind_for_ext("mp3"), "audio");
        assert_eq!(kind_for_ext("JPG"), "photo");
        assert_eq!(kind_for_ext("mp4"), "video");
        assert_eq!(kind_for_ext("txt"), "other");
    }

    #[test]
    fn scan_empty_roots_errors() {
        let res = scan_filesystem(Some(vec![]), false);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("ERR_LIBRARY_NO_ROOTS"));
    }

    #[test]
    fn scan_temp_dir() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.mp3"), b"fake mp3").unwrap();
        std::fs::write(dir.path().join("b.jpg"), b"fake jpg").unwrap();
        std::fs::write(dir.path().join("c.txt"), b"ignore").unwrap();
        let roots = Some(vec![dir.path().display().to_string()]);
        let res = scan_filesystem(roots, false).unwrap();
        assert_eq!(res.scanned, 2);
        assert_eq!(res.entries.len(), 2);
        assert!(res
            .entries
            .iter()
            .any(|e| e.name == "a.mp3" && e.kind == "audio"));
        assert!(res
            .entries
            .iter()
            .any(|e| e.name == "b.jpg" && e.kind == "photo"));
    }
}
