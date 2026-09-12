//! Action handlers for the `filesystem` plugin: `fs_list`, `fs_read`,
//! `fs_write`. Every action runs against the sandbox (allowed roots) and the
//! operator config (caps). Handlers return `Result<Value, String>`; errors
//! surface as `ACTION_ERROR` with a message naming the offending field.
//!
//! Pure parse → operate → respond structure: [`Handler::handle`] dispatches a
//! typed [`Request`] to a pure `fs_*` function that touches only the local
//! filesystem, so tests drive the whole flow without a live kernel.

use std::fs::{self, File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::config::{clamp_max_read, Config};
use crate::request::{
    parse_request, DeleteParams, ListParams, MkdirParams, MoveParams, ReadParams, RenameParams,
    Request, WriteParams,
};
use crate::sandbox::Sandbox;

pub struct Handler {
    sandbox: Sandbox,
    config: Config,
}

impl Handler {
    pub fn new(sandbox: Sandbox, config: Config) -> Self {
        Self { sandbox, config }
    }

    pub fn handle(&self, action: &str, params_json: &[u8]) -> Result<Value, String> {
        match parse_request(action, params_json)? {
            Request::List(p) => self.fs_list(&p),
            Request::Read(p) => self.fs_read(&p),
            Request::Write(p) => self.fs_write(&p),
            Request::Delete(p) => self.fs_delete(&p),
            Request::Mkdir(p) => self.fs_mkdir(&p),
            Request::Rename(p) => self.fs_rename(&p),
            Request::Move(p) => self.fs_move(&p),
        }
    }

    pub fn fs_list(&self, p: &ListParams) -> Result<Value, String> {
        let resolved = self.sandbox.resolve(Path::new(&p.path))?;
        let meta = fs::metadata(&resolved.path).map_err(|e| not_found_or_io(&p.path, e))?;
        if !meta.is_dir() {
            return Err(format!(
                "ERR_FILES_NOT_A_DIRECTORY: `path` ({}) is not a directory",
                p.path
            ));
        }

        let mut entries: Vec<Entry> = Vec::new();
        let read_dir =
            fs::read_dir(&resolved.path).map_err(|e| list_io(&p.path, e))?;
        for item in read_dir {
            let item = match item {
                Ok(i) => i,
                Err(_) => continue, // entry vanished mid-list; skip it
            };
            let name = item.file_name().to_string_lossy().into_owned();
            if !p.include_hidden && name.starts_with('.') {
                continue;
            }
            let lstat = fs::symlink_metadata(item.path()).ok();
            entries.push(Entry::from_lstat(name, lstat.as_ref()));
        }

        entries.sort_by(|a, b| rank(a.kind).cmp(&rank(b.kind)).then_with(|| a.name.cmp(&b.name)));
        let truncated = entries.len() > self.config.max_list_entries;
        entries.truncate(self.config.max_list_entries);
        let entries_json: Vec<Value> = entries.iter().map(Entry::to_json).collect();

        Ok(json!({
            "path": resolved.path.display().to_string(),
            "entries": entries_json,
            "truncated": truncated,
        }))
    }

    pub fn fs_read(&self, p: &ReadParams) -> Result<Value, String> {
        let resolved = self.sandbox.resolve(Path::new(&p.path))?;
        let meta = fs::metadata(&resolved.path).map_err(|e| not_found_or_io(&p.path, e))?;
        if meta.is_dir() {
            return Err(format!(
                "ERR_FILES_IS_A_DIRECTORY: `path` ({}) is a directory, not a file",
                p.path
            ));
        }
        let file_size = meta.len();
        let window = match p.max_bytes {
            Some(m) => clamp_max_read(m),
            None => clamp_max_read(self.config.max_read_bytes),
        };

        let mut file =
            File::open(&resolved.path).map_err(|e| open_io(&p.path, e))?;
        if p.offset > 0 {
            file.seek(SeekFrom::Start(p.offset)).map_err(|e| seek_io(&p.path, e))?;
        }
        let mut buf = vec![0u8; window as usize];
        let mut total = 0usize;
        while total < buf.len() {
            let n = file
                .read(&mut buf[total..])
                .map_err(|e| read_io(&p.path, e))?;
            if n == 0 {
                break;
            }
            total += n;
        }
        buf.truncate(total);

        let truncated = p.offset.saturating_add(total as u64) < file_size;
        // utf8 only when we read the whole file from the start and it is
        // valid UTF-8; every other case (partial read, offset > 0, binary) is
        // base64.
        let (data, encoding) = if p.offset == 0 && !truncated {
            match String::from_utf8(buf) {
                Ok(s) => (s, "utf8"),
                Err(e) => (encode_base64(&e.into_bytes()), "base64"),
            }
        } else {
            (encode_base64(&buf), "base64")
        };

        Ok(json!({
            "data": data,
            "encoding": encoding,
            "size_bytes": total,
            "truncated": truncated,
        }))
    }

    pub fn fs_write(&self, p: &WriteParams) -> Result<Value, String> {
        let resolved = self.sandbox.resolve(Path::new(&p.path))?;
        if resolved.is_root {
            return Err(format!(
                "ERR_FILES_IS_A_DIRECTORY: `path` ({}) is an allowed root; fs_write targets files only",
                p.path
            ));
        }
        if let Ok(m) = fs::symlink_metadata(&resolved.path) {
            if m.file_type().is_symlink() {
                return Err(format!(
                    "ERR_FILES_SYMLINK: refusing to write through symlink `path` ({})",
                    p.path
                ));
            }
            if m.is_dir() {
                return Err(format!(
                    "ERR_FILES_IS_A_DIRECTORY: `path` ({}) is a directory",
                    p.path
                ));
            }
        }

        let parent = resolved.path.parent().ok_or_else(|| {
            format!("ERR_FILES_PATH_UNRESOLVABLE: `path` ({}) has no parent", p.path)
        })?;
        if p.create_parents {
            fs::create_dir_all(parent).map_err(|e| create_parents_io(&p.path, e))?;
        } else if !parent.is_dir() {
            return Err(format!(
                "ERR_FILES_NOT_FOUND: parent directory of `path` ({}) does not exist; set create_parents=true to create it",
                p.path
            ));
        }

        fs::write(&resolved.path, &p.bytes).map_err(|e| write_io(&p.path, e))?;
        Ok(json!({
            "written_bytes": p.bytes.len(),
            "path": resolved.path.display().to_string(),
        }))
    }

    pub fn fs_delete(&self, p: &DeleteParams) -> Result<Value, String> {
        let resolved = self.sandbox.resolve(Path::new(&p.path))?;
        if resolved.is_root {
            return Err(format!("ERR_FILES_IS_A_DIRECTORY: refusing to delete allowed root {}", p.path));
        }
        let meta = fs::symlink_metadata(&resolved.path).map_err(|e| not_found_or_io(&p.path, e))?;
        // Do not follow symlink dir escapes - we already resolved via sandbox, but deleting symlink itself is fine
        if p.to_trash {
            trash_move(&resolved.path, &meta)?;
            Ok(json!({"deleted": true, "trashed": true, "path": resolved.path.display().to_string()}))
        } else {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(&resolved.path).map_err(|e| write_io(&p.path, e))?;
            } else {
                fs::remove_file(&resolved.path).map_err(|e| write_io(&p.path, e))?;
            }
            Ok(json!({"deleted": true, "trashed": false, "path": resolved.path.display().to_string()}))
        }
    }

    pub fn fs_mkdir(&self, p: &MkdirParams) -> Result<Value, String> {
        let resolved = self.sandbox.resolve(Path::new(&p.path))?;
        if resolved.path.exists() {
            let m = fs::symlink_metadata(&resolved.path).map_err(|e| not_found_or_io(&p.path, e))?;
            if m.is_dir() {
                return Ok(json!({"created": false, "path": resolved.path.display().to_string()}));
            } else {
                return Err(format!("ERR_FILES_EXISTS: path {} exists and is not a directory", p.path));
            }
        }
        if p.parents {
            fs::create_dir_all(&resolved.path).map_err(|e| create_parents_io(&p.path, e))?;
        } else {
            fs::create_dir(&resolved.path).map_err(|e| create_parents_io(&p.path, e))?;
        }
        Ok(json!({"created": true, "path": resolved.path.display().to_string()}))
    }

    pub fn fs_rename(&self, p: &RenameParams) -> Result<Value, String> {
        let from_resolved = self.sandbox.resolve(Path::new(&p.from))?;
        let to_resolved = self.sandbox.resolve(Path::new(&p.to))?;
        if from_resolved.is_root || to_resolved.is_root {
            return Err("ERR_FILES_IS_A_DIRECTORY: refusing to rename allowed root".into());
        }
        let from_meta = fs::symlink_metadata(&from_resolved.path).map_err(|e| not_found_or_io(&p.from, e))?;
        let _ = from_meta;
        if to_resolved.path.exists() && !p.overwrite {
            return Err(format!("ERR_FILES_EXISTS: destination {} already exists (set overwrite=true)", p.to));
        }
        if let Some(parent) = to_resolved.path.parent() {
            if !parent.is_dir() {
                return Err(format!("ERR_FILES_NOT_FOUND: destination parent {} does not exist", parent.display()));
            }
        }
        // If overwrite and destination exists, remove it first (file or dir)
        if p.overwrite && to_resolved.path.exists() {
            let m = fs::symlink_metadata(&to_resolved.path).map_err(|e| not_found_or_io(&p.to, e))?;
            if m.is_dir() && !m.file_type().is_symlink() {
                fs::remove_dir_all(&to_resolved.path).map_err(|e| write_io(&p.to, e))?;
            } else {
                fs::remove_file(&to_resolved.path).map_err(|e| write_io(&p.to, e))?;
            }
        }
        fs::rename(&from_resolved.path, &to_resolved.path).map_err(|e| write_io(&format!("{} -> {}", p.from, p.to), e))?;
        Ok(json!({"renamed": true, "from": from_resolved.path.display().to_string(), "to": to_resolved.path.display().to_string()}))
    }

    pub fn fs_move(&self, p: &MoveParams) -> Result<Value, String> {
        let from_resolved = self.sandbox.resolve(Path::new(&p.from))?;
        let to_resolved = self.sandbox.resolve(Path::new(&p.to))?;
        if from_resolved.is_root || to_resolved.is_root {
            return Err("ERR_FILES_IS_A_DIRECTORY: refusing to move allowed root".into());
        }
        let from_meta = fs::symlink_metadata(&from_resolved.path).map_err(|e| not_found_or_io(&p.from, e))?;
        let _ = from_meta;
        if to_resolved.path.exists() && !p.overwrite {
            return Err(format!("ERR_FILES_EXISTS: destination {} already exists (set overwrite=true)", p.to));
        }
        if let Some(parent) = to_resolved.path.parent() {
            if !parent.is_dir() {
                return Err(format!("ERR_FILES_NOT_FOUND: destination parent {} does not exist", parent.display()));
            }
        }
        // If overwrite and destination exists, remove it first (file or dir)
        if p.overwrite && to_resolved.path.exists() {
            let m = fs::symlink_metadata(&to_resolved.path).map_err(|e| not_found_or_io(&p.to, e))?;
            if m.is_dir() && !m.file_type().is_symlink() {
                fs::remove_dir_all(&to_resolved.path).map_err(|e| write_io(&p.to, e))?;
            } else {
                fs::remove_file(&to_resolved.path).map_err(|e| write_io(&p.to, e))?;
            }
        }
        fs::rename(&from_resolved.path, &to_resolved.path).map_err(|e| write_io(&format!("{} -> {}", p.from, p.to), e))?;
        Ok(json!({"moved": true, "from": from_resolved.path.display().to_string(), "to": to_resolved.path.display().to_string()}))
    }
}

/// A single directory entry, classified via `lstat` so symlinks report their
/// own kind rather than their target's.
struct Entry {
    name: String,
    kind: Kind,
    size_bytes: u64,
    modified_unix_ms: Option<u64>,
}

impl Entry {
    fn from_lstat(name: String, meta: Option<&Metadata>) -> Self {
        match meta {
            None => Entry {
                name,
                kind: Kind::Other,
                size_bytes: 0,
                modified_unix_ms: None,
            },
            Some(m) => {
                let kind = if m.file_type().is_symlink() {
                    Kind::Symlink
                } else if m.is_dir() {
                    Kind::Dir
                } else if m.is_file() {
                    Kind::File
                } else {
                    Kind::Other
                };
                let modified_unix_ms = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64);
                Entry {
                    name,
                    kind,
                    size_bytes: m.len(),
                    modified_unix_ms,
                }
            }
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "kind": self.kind.as_str(),
            "size_bytes": self.size_bytes,
            "modified_unix_ms": self.modified_unix_ms,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Dir => "dir",
            Kind::File => "file",
            Kind::Symlink => "symlink",
            Kind::Other => "other",
        }
    }
}

/// Dirs sort first, everything else by name.
fn rank(kind: Kind) -> u8 {
    match kind {
        Kind::Dir => 0,
        _ => 1,
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn not_found_or_io(path: &str, e: std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("ERR_FILES_NOT_FOUND: `path` ({path}) does not exist")
    } else {
        format!("ERR_FILES_IO: cannot stat {path}: {e}")
    }
}

fn list_io(path: &str, e: std::io::Error) -> String {
    format!("ERR_FILES_IO: cannot list {path}: {e}")
}

fn open_io(path: &str, e: std::io::Error) -> String {
    format!("ERR_FILES_IO: cannot open {path}: {e}")
}

fn seek_io(path: &str, e: std::io::Error) -> String {
    format!("ERR_FILES_IO: seek failed on {path}: {e}")
}

fn read_io(path: &str, e: std::io::Error) -> String {
    format!("ERR_FILES_IO: read failed on {path}: {e}")
}

fn create_parents_io(path: &str, e: std::io::Error) -> String {
    format!("ERR_FILES_IO: cannot create parent directories for {path}: {e}")
}

fn write_io(path: &str, e: std::io::Error) -> String {
    format!("ERR_FILES_IO: write failed on {path}: {e}")
}

fn trash_move(original: &Path, _meta: &Metadata) -> Result<(), String> {
    let trash_base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(|p| Path::new(&p).join("Trash"))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            Path::new(&home).join(".local/share/Trash")
        });
    let files_dir = trash_base.join("files");
    let info_dir = trash_base.join("info");
    fs::create_dir_all(&files_dir).map_err(|e| format!("ERR_FILES_IO: cannot create trash files dir: {e}"))?;
    fs::create_dir_all(&info_dir).map_err(|e| format!("ERR_FILES_IO: cannot create trash info dir: {e}"))?;
    let file_name = original.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    let mut dest = files_dir.join(file_name);
    let mut counter = 0;
    while dest.exists() {
        counter += 1;
        let new_name = format!("{file_name}.{counter}");
        dest = files_dir.join(new_name);
    }
    fs::rename(original, &dest).map_err(|e| format!("ERR_FILES_IO: trash move failed: {e}"))?;
    let info_name = dest.file_name().and_then(|n| n.to_str()).unwrap_or(file_name);
    let info_path = info_dir.join(format!("{info_name}.trashinfo"));
    let deletion_date = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let original_str = original.display().to_string();
    let info_content = format!("[Trash Info]\nPath={original_str}\nDeletionDate={deletion_date}\n");
    fs::write(&info_path, info_content).map_err(|e| format!("ERR_FILES_IO: cannot write trashinfo: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn handler_with_root(dir: &Path, max_list_entries: usize) -> Handler {
        Handler::new(
            Sandbox::from_raw_roots(&[dir.display().to_string()]),
            Config {
                max_list_entries,
                max_read_bytes: 1024 * 1024,
            },
        )
    }

    fn list(h: &Handler, dir: &Path, include_hidden: bool) -> Value {
        h.fs_list(&ListParams {
            path: dir.display().to_string(),
            include_hidden,
        })
        .unwrap()
    }

    fn names(out: &Value) -> Vec<&str> {
        out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect()
    }


    #[test]
    fn list_sorts_dirs_first_and_hides_dotfiles_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"y").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"z").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let out = list(&h, dir.path(), false);
        assert_eq!(names(&out), ["sub", "a.txt", "b.txt"]);
        assert_eq!(out["truncated"], false);

        let out = list(&h, dir.path(), true);
        assert_eq!(names(&out), ["sub", ".hidden", "a.txt", "b.txt"]);
    }

    #[test]
    fn list_reports_symlink_kind_via_lstat() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let out = list(&h, dir.path(), false);
        assert_eq!(names(&out), ["f.txt", "link"]);
        assert_eq!(out["entries"][1]["kind"], "symlink");
    }

    #[test]
    fn list_caps_entries_and_sets_truncated() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}.txt")), b"x").unwrap();
        }
        let h = handler_with_root(dir.path(), 3);
        let out = list(&h, dir.path(), false);
        assert_eq!(out["entries"].as_array().unwrap().len(), 3);
        assert_eq!(out["truncated"], true);
    }

    #[test]
    fn list_empty_dir_returns_empty_entries() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = list(&h, dir.path(), false);
        assert_eq!(out["entries"].as_array().unwrap().len(), 0);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn list_rejects_file_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_list(&ListParams {
                path: dir.path().join("f.txt").display().to_string(),
                include_hidden: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_NOT_A_DIRECTORY"), "{err}");
    }


    #[test]
    fn read_returns_utf8_when_whole_file_fits_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.txt"), "héllo").unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("t.txt").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap();
        assert_eq!(out["encoding"], "utf8");
        assert_eq!(out["data"], "héllo");
        assert_eq!(out["size_bytes"], 6); // é is two bytes
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn read_returns_base64_for_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.bin"), [0xFFu8, 0x00, 0xFE]).unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("b.bin").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap();
        assert_eq!(out["encoding"], "base64");
        assert_eq!(out["size_bytes"], 3);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn read_offset_forces_base64_even_for_text() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.txt"), b"hello").unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("t.txt").display().to_string(),
                offset: 1,
                max_bytes: None,
            })
            .unwrap();
        assert_eq!(out["encoding"], "base64");
        assert_eq!(out["size_bytes"], 4);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn read_window_sets_truncation_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.bin"), [0u8; 10]).unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("t.bin").display().to_string(),
                offset: 0,
                max_bytes: Some(4),
            })
            .unwrap();
        assert_eq!(out["size_bytes"], 4);
        assert_eq!(out["truncated"], true);

        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("t.bin").display().to_string(),
                offset: 8,
                max_bytes: Some(4),
            })
            .unwrap();
        assert_eq!(out["size_bytes"], 2);
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn read_oversized_window_is_clamped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("t.txt"), b"tiny").unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("t.txt").display().to_string(),
                offset: 0,
                max_bytes: Some(u64::MAX),
            })
            .unwrap();
        assert_eq!(out["data"], "tiny");
        assert_eq!(out["truncated"], false);
    }

    #[test]
    fn read_missing_file_names_the_field() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_read(&ReadParams {
                path: dir.path().join("nope").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_NOT_FOUND") && err.contains("`path`"), "{err}");
    }

    #[test]
    fn read_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_read(&ReadParams {
                path: dir.path().display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_IS_A_DIRECTORY"), "{err}");
    }


    #[test]
    fn write_text_creates_and_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let target = dir.path().join("out.txt");
        for content in ["first", "second"] {
            let out = h
                .fs_write(&WriteParams {
                    path: target.display().to_string(),
                    bytes: content.as_bytes().to_vec(),
                    create_parents: false,
                })
                .unwrap();
            assert_eq!(out["written_bytes"], content.len());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), content);
        }
    }

    #[test]
    fn write_base64_round_trips_binary() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let target = dir.path().join("b.bin");
        h.fs_write(&WriteParams {
            path: target.display().to_string(),
            bytes: vec![0xFF, 0x00, 0xFE],
            create_parents: false,
        })
        .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), vec![0xFFu8, 0x00, 0xFE]);
    }

    #[test]
    fn write_create_parents_gates_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let target = dir.path().join("a/b/c.txt");

        let err = h
            .fs_write(&WriteParams {
                path: target.display().to_string(),
                bytes: b"x".to_vec(),
                create_parents: false,
            })
            .unwrap_err();
        assert!(err.contains("create_parents=true"), "{err}");

        h.fs_write(&WriteParams {
            path: target.display().to_string(),
            bytes: b"x".to_vec(),
            create_parents: true,
        })
        .unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }

    #[test]
    fn write_onto_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_write(&WriteParams {
                path: dir.path().join("sub").display().to_string(),
                bytes: b"x".to_vec(),
                create_parents: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_IS_A_DIRECTORY"), "{err}");
    }

    #[test]
    fn write_onto_a_root_itself_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_write(&WriteParams {
                path: dir.path().display().to_string(),
                bytes: b"x".to_vec(),
                create_parents: false,
            })
            .unwrap_err();
        assert!(err.contains("targets files only"), "{err}");
    }


    #[test]
    fn read_through_file_symlink_outside_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, b"top secret").unwrap();
        symlink(&secret, dir.path().join("innocent")).unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_read(&ReadParams {
                path: dir.path().join("innocent").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap_err();
        assert!(
            err.contains("ERR_FILES_PATH_ESCAPES_ROOT"),
            "expected escape rejection, got: {err}"
        );
    }

    #[test]
    fn read_through_symlinked_dir_component_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("s.txt"), b"s").unwrap();
        symlink(outside.path(), dir.path().join("linkdir")).unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_read(&ReadParams {
                path: dir.path().join("linkdir/s.txt").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_PATH_ESCAPES_ROOT"), "{err}");

        let err = h
            .fs_list(&ListParams {
                path: dir.path().join("linkdir").display().to_string(),
                include_hidden: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_PATH_ESCAPES_ROOT"), "{err}");
    }

    #[test]
    fn write_through_dangling_symlink_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path().join("target"), dir.path().join("dangling")).unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_write(&WriteParams {
                path: dir.path().join("dangling").display().to_string(),
                bytes: b"x".to_vec(),
                create_parents: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_SYMLINK"), "{err}");
        // The symlink must not have been followed into a real file.
        assert!(!outside.path().join("target").exists());
    }

    #[test]
    fn absolute_path_outside_every_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_read(&ReadParams {
                path: other.path().join("x").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_PATH_ESCAPES_ROOT"), "{err}");
    }

    #[test]
    fn traversal_that_stays_inside_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("top.txt"), b"t").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        // sub/../top.txt canonicalizes fully inside the root — allowed.
        let out = h
            .fs_read(&ReadParams {
                path: dir.path().join("sub/../top.txt").display().to_string(),
                offset: 0,
                max_bytes: None,
            })
            .unwrap();
        assert_eq!(out["data"], "t");
    }


    #[test]
    fn delete_existing_file_to_trash_moves_it_out() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("victim.txt");
        std::fs::write(&file, b"bye").unwrap();
        // Confine freedesktop Trash to the tempdir so tests never touch $HOME.
        std::env::set_var("XDG_DATA_HOME", dir.path().join("xdg").display().to_string());

        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_delete(&DeleteParams {
                path: file.display().to_string(),
                to_trash: true,
            })
            .unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["trashed"], true);
        assert!(!file.exists(), "source file should be gone after trashing");
    }

    #[test]
    fn delete_permanently_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("gone.txt");
        std::fs::write(&file, b"x").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_delete(&DeleteParams {
                path: file.display().to_string(),
                to_trash: false,
            })
            .unwrap();
        assert_eq!(out["deleted"], true);
        assert_eq!(out["trashed"], false);
        assert!(!file.exists());
    }

    #[test]
    fn delete_non_existent_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_delete(&DeleteParams {
                path: dir.path().join("nope.txt").display().to_string(),
                to_trash: true,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_NOT_FOUND"), "{err}");
    }

    #[test]
    fn delete_root_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_delete(&DeleteParams {
                path: dir.path().display().to_string(),
                to_trash: true,
            })
            .unwrap_err();
        assert!(err.contains("refusing to delete allowed root"), "{err}");
    }


    #[test]
    fn mkdir_creates_directory() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let target = dir.path().join("newdir");
        let out = h
            .fs_mkdir(&MkdirParams {
                path: target.display().to_string(),
                parents: false,
            })
            .unwrap();
        assert_eq!(out["created"], true);
        assert!(target.is_dir());
    }

    #[test]
    fn mkdir_with_parents_creates_nested_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let target = dir.path().join("a/b/c");
        let out = h
            .fs_mkdir(&MkdirParams {
                path: target.display().to_string(),
                parents: true,
            })
            .unwrap();
        assert_eq!(out["created"], true);
        assert!(target.is_dir());
    }

    #[test]
    fn mkdir_existing_dir_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_mkdir(&MkdirParams {
                path: dir.path().join("sub").display().to_string(),
                parents: false,
            })
            .unwrap();
        assert_eq!(out["created"], false);
    }

    #[test]
    fn mkdir_onto_existing_file_is_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"x").unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_mkdir(&MkdirParams {
                path: dir.path().join("f.txt").display().to_string(),
                parents: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_EXISTS"), "{err}");
    }


    #[test]
    fn rename_moves_file_within_root() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("b.txt");
        std::fs::write(&src, b"content").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_rename(&RenameParams {
                from: src.display().to_string(),
                to: dst.display().to_string(),
                overwrite: false,
            })
            .unwrap();
        assert_eq!(out["renamed"], true);
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "content");
    }

    #[test]
    fn rename_across_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("sub/a.txt");
        std::fs::write(&src, b"moved").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_rename(&RenameParams {
                from: src.display().to_string(),
                to: dst.display().to_string(),
                overwrite: false,
            })
            .unwrap();
        assert_eq!(out["renamed"], true);
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "moved");
    }

    #[test]
    fn rename_overwrite_flag_gates_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_rename(&RenameParams {
                from: src.display().to_string(),
                to: dst.display().to_string(),
                overwrite: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_EXISTS"), "{err}");

        h.fs_rename(&RenameParams {
            from: src.display().to_string(),
            to: dst.display().to_string(),
            overwrite: true,
        })
        .unwrap();
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
    }


    #[test]
    fn move_file_across_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let src = dir.path().join("a.txt");
        let dst = dir.path().join("sub/b.txt");
        std::fs::write(&src, b"relocated").unwrap();

        let h = handler_with_root(dir.path(), 1000);
        let out = h
            .fs_move(&MoveParams {
                from: src.display().to_string(),
                to: dst.display().to_string(),
                overwrite: false,
            })
            .unwrap();
        assert_eq!(out["moved"], true);
        assert!(!src.exists());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "relocated");
    }

    #[test]
    fn move_non_existent_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let h = handler_with_root(dir.path(), 1000);
        let err = h
            .fs_move(&MoveParams {
                from: dir.path().join("missing.txt").display().to_string(),
                to: dir.path().join("dest.txt").display().to_string(),
                overwrite: false,
            })
            .unwrap_err();
        assert!(err.contains("ERR_FILES_NOT_FOUND"), "{err}");
    }
}
