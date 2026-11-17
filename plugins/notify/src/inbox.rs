//! Hidden-notification inbox: a plaintext JSON store under
//! `{NOTIFY_PLUGIN_DATA_DIR}/inbox.json`.
//!
//! Silent notifications (`silent: true`) land here without any delivery;
//! delivered notifications also record an audit entry (best-effort). The
//! data dir is resolved lazily — a push-only deployment without
//! `NOTIFY_PLUGIN_DATA_DIR` keeps working, only the inbox features are
//! unavailable.
//!
//! Persistence follows the `secrets` vault discipline: writes are atomic
//! (temp file → fsync → rename → fsync dir, mode 0600) and a corrupt inbox
//! file fails loudly — it is never silently reset or returned as empty.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Env var holding the inbox data directory. Required only for silent
/// notifications and the `notify_list` / `notify_mark_read` /
/// `notify_delete` actions.
pub const DATA_DIR_ENV: &str = "NOTIFY_PLUGIN_DATA_DIR";
/// Inbox file name inside the data dir.
const INBOX_FILE: &str = "inbox.json";
/// Entries kept in the inbox; `push` prunes to the newest this many.
pub const MAX_ENTRIES: usize = 500;

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Resolve the inbox data dir from [`DATA_DIR_ENV`], creating it when
/// present. Unset → error (the caller decides whether that is fatal — for
/// delivered notifications it is not).
pub fn data_dir() -> Result<PathBuf, String> {
    let raw = std::env::var(DATA_DIR_ENV)
        .or_else(|_| std::env::var("VYN_DATA_DIR"))
        .map_err(|_| {
            format!("{DATA_DIR_ENV} is not set — required for silent notifications / the inbox")
        })?;
    let dir = PathBuf::from(raw);
    fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create data dir {}: {e}", dir.display()))?;
    Ok(dir)
}

fn inbox_path() -> Result<PathBuf, String> {
    Ok(data_dir()?.join(INBOX_FILE))
}

/// One stored notification (silent or delivered audit entry).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxEntry {
    /// `{created_at_ms}-{seq}`, assigned by [`Inbox::push`].
    pub id: String,
    /// Unix milliseconds when the entry was created.
    pub created_at_ms: u64,
    pub title: String,
    pub message: String,
    /// Provider id the notification would have gone through (`notify-send`,
    /// `wall`, `espeak`).
    pub provider: String,
    /// True when the notification was actually delivered; silent entries
    /// are store-only.
    pub delivered: bool,
    pub silent: bool,
    /// True when the tts озвучка succeeded (`speak: true` path).
    pub spoken: bool,
    pub read: bool,
}

/// The inbox store: a loaded `Vec<InboxEntry>` bound to its file. The serve
/// loop is strictly sequential, so a plain owned [`Inbox`] in loop state is
/// enough — no locking.
#[derive(Debug)]
pub struct Inbox {
    path: PathBuf,
    entries: Vec<InboxEntry>,
    next_seq: u64,
}

fn parse_seq(id: &str) -> u64 {
    id.rsplit_once('-')
        .and_then(|(_, seq)| seq.parse().ok())
        .unwrap_or(0)
}

impl Inbox {
    /// Open the inbox at `{NOTIFY_PLUGIN_DATA_DIR}/inbox.json`, loading the
    /// existing entries or starting empty when the file does not exist yet.
    /// A corrupt file is an error — never a silent reset.
    pub fn open() -> Result<Inbox, String> {
        Self::open_at(inbox_path()?)
    }

    /// [`Inbox::open`] against an explicit path — the env-free entry point
    /// used by tests (and any crate-internal caller) to avoid depending on
    /// the ambient environment.
    pub(crate) fn open_at(path: PathBuf) -> Result<Inbox, String> {
        let entries: Vec<InboxEntry> = match fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| format!("inbox file {} is corrupt: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("failed to read inbox {}: {e}", path.display())),
        };
        let next_seq = entries
            .iter()
            .map(|e| parse_seq(&e.id))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        Ok(Inbox {
            path,
            entries,
            next_seq,
        })
    }

    /// Append an entry: assign `created_at_ms` + `id`, prune to the newest
    /// [`MAX_ENTRIES`], persist atomically, and return the assigned id.
    pub fn push(&mut self, mut entry: InboxEntry) -> Result<String, String> {
        entry.created_at_ms = unix_millis();
        let id = format!("{}-{}", entry.created_at_ms, self.next_seq);
        entry.id = id.clone();
        self.next_seq = self.next_seq.saturating_add(1);
        self.entries.push(entry);
        self.entries = self
            .entries
            .split_off(self.entries.len().saturating_sub(MAX_ENTRIES));
        self.persist()?;
        Ok(id)
    }

    /// Newest-first listing; filters out read entries unless `include_read`.
    pub fn list(&self, include_read: bool) -> Vec<InboxEntry> {
        let mut out: Vec<InboxEntry> = self
            .entries
            .iter()
            .filter(|e| include_read || !e.read)
            .cloned()
            .collect();
        out.reverse();
        out
    }

    /// Mark an entry read. Returns true only when the entry existed and was
    /// unread (i.e. state actually changed); persists only on change.
    pub fn mark_read(&mut self, id: &str) -> Result<bool, String> {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.id == id {
                if !entry.read {
                    entry.read = true;
                    changed = true;
                }
                break;
            }
        }
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    /// Delete an entry. Returns true when it existed; persists only on
    /// change.
    pub fn delete(&mut self, id: &str) -> Result<bool, String> {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        let changed = self.entries.len() != before;
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    fn persist(&self) -> Result<(), String> {
        let json = serde_json::to_vec_pretty(&self.entries)
            .map_err(|e| format!("failed to serialize inbox: {e}"))?;
        atomic_write(&self.path, &json)
    }
}

/// Write `raw` to `path` atomically: temp file in the same dir → fsync →
/// rename → fsync dir. Mode 0600 on creation.
fn atomic_write(path: &Path, raw: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("inbox path {} has no parent dir", path.display()))?;
    let tmp_path = path.with_extension("json.tmp");
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    {
        let mut f = opts
            .open(&tmp_path)
            .map_err(|e| format!("failed to create {}: {e}", tmp_path.display()))?;
        f.write_all(raw)
            .map_err(|e| format!("failed to write {}: {e}", tmp_path.display()))?;
        f.sync_all()
            .map_err(|e| format!("failed to fsync {}: {e}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path).map_err(|e| {
        format!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        )
    })?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: &str, message: &str) -> InboxEntry {
        InboxEntry {
            id: String::new(),
            created_at_ms: 0,
            title: title.to_string(),
            message: message.to_string(),
            provider: "notify-send".to_string(),
            delivered: true,
            silent: false,
            spoken: false,
            read: false,
        }
    }

    fn open_temp(dir: &tempfile::TempDir) -> Inbox {
        Inbox::open_at(dir.path().join("inbox.json")).unwrap()
    }

    #[test]
    fn push_assigns_id_and_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = open_temp(&dir);
        let id1 = inbox.push(entry("t1", "m1")).unwrap();
        let id2 = inbox.push(entry("t2", "m2")).unwrap();
        assert!(!id1.is_empty());
        assert!(!id2.is_empty());
        assert_ne!(id1, id2);
        assert!(id1.ends_with("-1"), "first id was: {id1}");
        assert!(id2.ends_with("-2"), "second id was: {id2}");

        let inbox2 = open_temp(&dir);
        let all = inbox2.list(true);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, id2, "newest first");
        assert_eq!(all[1].id, id1);
        assert!(all[0].created_at_ms > 0);
    }

    #[test]
    fn list_filters_read_when_include_read_is_false() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = open_temp(&dir);
        let id1 = inbox.push(entry("t1", "m1")).unwrap();
        let id2 = inbox.push(entry("t2", "m2")).unwrap();

        assert_eq!(inbox.list(false).len(), 2);
        assert!(inbox.mark_read(&id2).unwrap());
        let unread = inbox.list(false);
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].id, id1);
        assert_eq!(inbox.list(true).len(), 2);
    }

    #[test]
    fn mark_read_persists_and_reports_change_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = open_temp(&dir);
        let id = inbox.push(entry("t1", "m1")).unwrap();

        assert!(inbox.mark_read(&id).unwrap());
        assert!(!inbox.mark_read(&id).unwrap(), "already read = no change");
        assert!(!inbox.mark_read("no-such-id").unwrap());

        let inbox2 = open_temp(&dir);
        assert!(inbox2.list(true)[0].read, "read flag persisted");
    }

    #[test]
    fn delete_persists_and_reports_found_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = open_temp(&dir);
        let id = inbox.push(entry("t1", "m1")).unwrap();

        assert!(inbox.delete(&id).unwrap());
        assert!(!inbox.delete(&id).unwrap(), "already gone = no change");
        assert!(!inbox.delete("no-such-id").unwrap());

        let inbox2 = open_temp(&dir);
        assert!(inbox2.list(true).is_empty(), "deletion persisted");
    }

    #[test]
    fn push_prunes_to_newest_max_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = open_temp(&dir);
        let mut ids = Vec::new();
        for i in 0..(MAX_ENTRIES + 5) {
            ids.push(inbox.push(entry("t", &format!("m{i}"))).unwrap());
        }
        let all = inbox.list(true);
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all[0].id, ids[MAX_ENTRIES + 4], "newest kept");
        assert!(!all.iter().any(|e| e.id == ids[0]), "oldest entries pruned");

        let inbox2 = open_temp(&dir);
        assert_eq!(inbox2.list(true).len(), MAX_ENTRIES, "prune persisted");
    }

    #[test]
    fn corrupt_inbox_file_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.json");
        fs::write(&path, b"{not json").unwrap();
        let err = Inbox::open_at(path).unwrap_err();
        assert!(err.contains("corrupt"), "error was: {err}");
    }

    #[test]
    fn persist_is_atomic_no_tmp_file_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mut inbox = open_temp(&dir);
        inbox.push(entry("t1", "m1")).unwrap();
        assert!(
            !dir.path().join("inbox.json.tmp").exists(),
            "tmp file cleaned up after rename"
        );
    }
}
