//! Filesystem sandbox — the security boundary of the `filesystem` plugin.
//!
//! The plugin may only touch paths inside a configured allowlist of absolute
//! directory roots (`FILES_PLUGIN_ALLOWED_ROOTS`, comma-separated). Every
//! requested path is resolved by canonicalizing its deepest *existing*
//! ancestor (which resolves every symlink component along the way), textually
//! re-appending the non-existing remainder, and verifying the result stays
//! inside one of the roots. That blocks `..` traversal and symlink escapes
//! (file symlinks pointing outside a root, symlinked directory components).
//! Check-then-use TOCTOU is a documented non-goal (see ROADMAP.md).

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::config::ALLOWED_ROOTS_ENV;

/// Parsed, canonicalized, deduplicated allowlist of roots. Empty = deny-all.
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    roots: Vec<PathBuf>,
}

/// A requested path resolved against the sandbox.
#[derive(Debug)]
pub struct ResolvedPath {
    /// Absolute path with every existing component canonicalized and the
    /// non-existing remainder joined textually.
    pub path: PathBuf,
    /// True when the resolved path is exactly one of the allowed roots.
    pub is_root: bool,
}

impl Sandbox {
    /// Build the sandbox from `FILES_PLUGIN_ALLOWED_ROOTS`.
    pub fn from_env() -> Self {
        let raw = env_roots();
        Self::from_raw_roots(&raw)
    }

    /// Parse and canonicalize roots. Relative and nonexistent roots are
    /// logged loudly and skipped.
    pub fn from_raw_roots(raw: &[String]) -> Self {
        let mut roots: Vec<PathBuf> = Vec::new();
        for entry in raw {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            let path = PathBuf::from(entry);
            if !path.is_absolute() {
                eprintln!("[filesystem] ignoring non-absolute allowed root: {entry}");
                continue;
            }
            match path.canonicalize() {
                Ok(canon) => {
                    if !roots.contains(&canon) {
                        roots.push(canon);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[filesystem] ignoring nonexistent allowed root {}: {e}",
                        path.display()
                    );
                }
            }
        }
        roots.sort();
        Sandbox { roots }
    }

    /// The canonical roots (empty when deny-all). Exposed for tests and
    /// diagnostics.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Resolve `requested` and verify it stays inside an allowed root.
    ///
    /// Algorithm: require an absolute path; canonicalize the deepest existing
    /// ancestor (resolving every symlink in the existing portion); reject a
    /// `..` that survives into the non-existing remainder; textually join the
    /// remainder; require the result to be component-wise inside a root.
    pub fn resolve(&self, requested: &Path) -> Result<ResolvedPath, String> {
        if self.roots.is_empty() {
            return Err(deny_all_error());
        }
        if !requested.is_absolute() {
            return Err(format!(
                "ERR_FILES_PATH_MUST_BE_ABSOLUTE: `path` ({}) must be an absolute path inside an allowed root",
                requested.display()
            ));
        }

        let mut ancestor = requested.to_path_buf();
        let mut suffix: Vec<OsString> = Vec::new();
        let canonical_ancestor = loop {
            match ancestor.canonicalize() {
                Ok(canon) => break canon,
                Err(_) => {
                    // `file_name()` is None for paths terminating in `..` (or
                    // at the filesystem root); a trailing `..` on a
                    // non-existing ancestor is exactly the traversal case.
                    match ancestor.file_name().map(OsStr::to_os_string) {
                        Some(name) if name != OsStr::new("..") => suffix.push(name),
                        _ => {
                            if ancestor.components().next_back() == Some(Component::ParentDir) {
                                return Err(format!(
                                    "ERR_FILES_PATH_TRAVERSAL: `path` ({}) escapes the allowed roots via '..'",
                                    requested.display()
                                ));
                            }
                            return Err(unresolvable(requested));
                        }
                    }
                    if !ancestor.pop() {
                        return Err(unresolvable(requested));
                    }
                }
            }
        };

        let mut resolved = canonical_ancestor;
        for name in suffix.iter().rev() {
            resolved.push(name);
        }

        for root in &self.roots {
            if resolved.starts_with(root) {
                return Ok(ResolvedPath {
                    is_root: resolved.as_path() == root.as_path(),
                    path: resolved,
                });
            }
        }
        Err(format!(
            "ERR_FILES_PATH_ESCAPES_ROOT: `path` ({}) resolves outside every allowed root",
            requested.display()
        ))
    }
}

fn env_roots() -> Vec<String> {
    std::env::var(ALLOWED_ROOTS_ENV)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn unresolvable(requested: &Path) -> String {
    format!(
        "ERR_FILES_PATH_UNRESOLVABLE: cannot resolve `path` ({})",
        requested.display()
    )
}

fn deny_all_error() -> String {
    format!(
        "ERR_FILES_NO_ROOTS: {ALLOWED_ROOTS_ENV} is not set — configure at least one absolute \
         directory the filesystem plugin may access (see config.example.yaml)",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_roots_are_deny_all() {
        let sandbox = Sandbox::from_raw_roots(&[]);
        assert!(sandbox.roots().is_empty());
        let err = sandbox.resolve(Path::new("/tmp/x")).unwrap_err();
        assert!(err.contains("ERR_FILES_NO_ROOTS"), "{err}");
    }

    #[test]
    fn relative_and_nonexistent_roots_are_skipped() {
        let sandbox = Sandbox::from_raw_roots(&[
            "relative/path".to_string(),
            "/nonexistent/deadbeef/xyz".to_string(),
        ]);
        assert!(sandbox.roots().is_empty());
    }

    #[test]
    fn resolve_rejects_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::from_raw_roots(&[dir.path().display().to_string()]);
        let err = sandbox
            .resolve(Path::new("some/relative/path"))
            .unwrap_err();
        assert!(err.contains("ERR_FILES_PATH_MUST_BE_ABSOLUTE"), "{err}");
    }

    #[test]
    fn resolve_allows_paths_inside_root_and_marks_root() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::from_raw_roots(&[dir.path().display().to_string()]);
        let root = sandbox.resolve(dir.path()).unwrap();
        assert!(root.is_root);

        let inner = sandbox.resolve(&dir.path().join("a/b.txt")).unwrap();
        assert!(!inner.is_root);
        assert!(inner.path.starts_with(dir.path()));
    }

    #[test]
    fn resolve_rejects_traversal_that_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = Sandbox::from_raw_roots(&[dir.path().display().to_string()]);
        // "a/../.." — the leading 'a' doesn't exist, so '..' survives into
        // the non-existing remainder and is rejected before any join.
        let err = sandbox.resolve(&dir.path().join("a/../..")).unwrap_err();
        assert!(err.contains("ERR_FILES_PATH_TRAVERSAL"), "{err}");
    }
}
