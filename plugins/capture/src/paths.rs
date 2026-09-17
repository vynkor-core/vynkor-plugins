//! Output directory + filename generation for captured artifacts.
//! `CAPTURE_PLUGIN_DIR` overrides the default; the directory is created
//! on first use (`fs::create_dir_all`, idempotent).

use std::path::PathBuf;

pub const DIR_ENV: &str = "CAPTURE_PLUGIN_DIR";

/// Resolve (and ensure) the output directory. Never fails the caller on a
/// create error — `fs::create_dir_all`'s `Result` is surfaced by the first
/// actual file write instead, keeping this a pure path-resolution fn.
pub fn data_dir() -> PathBuf {
    let dir = std::env::var(DIR_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn default_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/vynkor/capture")
}

pub fn screenshot_filename(ext: &str) -> String {
    format!("screenshot-{}.{ext}", unix_millis())
}

pub fn record_filename(ext: &str) -> String {
    format!("record-{}.{ext}", unix_millis())
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // std::env::set_var races across parallel test threads within this
    // process; serialize the env-mutating tests in this module only.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn data_dir_honors_override_and_creates_it() {
        let _g = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("nested/capture");
        std::env::set_var(DIR_ENV, &target);
        let got = data_dir();
        std::env::remove_var(DIR_ENV);
        assert_eq!(got, target);
        assert!(target.is_dir());
    }

    #[test]
    fn filenames_carry_extension_and_prefix() {
        let s = screenshot_filename("png");
        assert!(s.starts_with("screenshot-") && s.ends_with(".png"), "{s}");
        let r = record_filename("mp4");
        assert!(r.starts_with("record-") && r.ends_with(".mp4"), "{r}");
    }
}
