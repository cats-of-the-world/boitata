// Optional workspace-root confinement for path-taking tools.
//
// Boitata is a coding agent that operates on the user's files, so by default its
// tools can touch any path the process can (the containment story is deployment-
// level isolation — a container or devbox, per the project's Minions model).
//
// When a `workspace_root` is configured, path-taking tools (file_read/write,
// list_directory, search) confine their target to that root: absolute paths
// outside it, `..` traversal, and symlinks that escape it are rejected. This is
// applied consistently across all such tools rather than special-casing one.

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use crate::tools::{Result, ToolError};

/// The configured workspace root, canonicalized. `None` (unset) means unconfined.
static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Set the workspace root once, at startup. Passing `None` leaves tools
/// unconfined. Calling more than once is a no-op (the first value wins).
pub fn init(root: Option<PathBuf>) {
    // Canonicalize so later `starts_with` checks compare resolved paths; if the
    // root doesn't exist yet, keep it as-is (confinement will still reject
    // escapes lexically).
    let canonical = root.map(|r| std::fs::canonicalize(&r).unwrap_or(r));
    let _ = ROOT.set(canonical);
}

/// Confine `path` to the configured workspace root, returning the path to
/// actually use. Unconfined when no root is set. Errors if `path` resolves
/// outside the root.
pub fn confine(path: &str) -> Result<PathBuf> {
    let root = ROOT.get().and_then(|o| o.as_ref());
    confine_within(root.map(PathBuf::as_path), path)
}

/// Pure core of [`confine`], parameterized on the root for testability.
fn confine_within(root: Option<&Path>, path: &str) -> Result<PathBuf> {
    let Some(root) = root else {
        return Ok(PathBuf::from(path));
    };

    let requested = Path::new(path);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    // Resolve `.`/`..` lexically first, then resolve symlinks in the portion of
    // the path that exists — either could otherwise escape the root.
    let normalized = lexical_normalize(&joined);
    let resolved = resolve_existing_prefix(&normalized);

    if resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(ToolError::ExecutionFailed(format!(
            "path `{path}` resolves outside the workspace root"
        )))
    }
}

/// Collapse `.` and `..` components without touching the filesystem.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the longest existing ancestor (resolving symlinks) and re-append
/// the non-existent tail, so a symlink in the path can't escape unnoticed while
/// still allowing not-yet-created files (e.g. `file_write`).
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    for ancestor in path.ancestors() {
        if let Ok(canonical) = std::fs::canonicalize(ancestor) {
            return match path.strip_prefix(ancestor) {
                Ok(tail) => canonical.join(tail),
                Err(_) => canonical,
            };
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unconfined_passes_through() {
        assert_eq!(
            confine_within(None, "/etc/passwd").unwrap(),
            PathBuf::from("/etc/passwd")
        );
    }

    #[test]
    fn test_allows_paths_within_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();

        // Relative path is joined onto the root.
        let resolved = confine_within(Some(&root), "a.txt").unwrap();
        assert!(resolved.starts_with(&root));

        // A not-yet-existing file inside the root is allowed (for writes).
        let new = confine_within(Some(&root), "sub/new.txt").unwrap();
        assert!(new.starts_with(&root));
    }

    #[test]
    fn test_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();

        // Absolute path outside the root.
        assert!(confine_within(Some(&root), "/etc").is_err());
        // `..` traversal out of the root.
        assert!(confine_within(Some(&root), "../../etc/passwd").is_err());
    }
}
