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
    let resolved = root.map(|r| {
        // Canonicalize so later `starts_with` checks compare resolved (symlink-
        // free) paths. If the root doesn't exist yet, fall back to a lexically
        // absolute form — an *absolute* root is what keeps the containment check
        // sound, so never leave it relative.
        std::fs::canonicalize(&r).unwrap_or_else(|_| {
            tracing::warn!(
                "could not canonicalize workspace root `{}`; using its absolute lexical path (symlinks in the root won't be resolved)",
                r.display()
            );
            std::path::absolute(&r).unwrap_or(r)
        })
    });
    if ROOT.set(resolved).is_err() {
        tracing::warn!("workspace root already initialized; ignoring repeat init()");
    }
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
///
/// A `..` that would escape the accumulated path (empty buffer, or a leading
/// `..` on a relative path) is *retained* rather than silently swallowed, so the
/// caller's containment check rejects the escape instead of accepting a
/// wrongly-normalized path. At a filesystem root, `..` is dropped (you can't go
/// above `/`).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => { /* at fs root; drop */ }
                // Empty buffer or a trailing `..` — keep the `..` so it's visible
                // to the containment check.
                _ => out.push(Component::ParentDir.as_os_str()),
            },
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
        // Leading `..` traversal out of the root.
        assert!(confine_within(Some(&root), "../../etc/passwd").is_err());
        // Over-pop: descend then escape past the root.
        assert!(confine_within(Some(&root), "sub/../../etc/passwd").is_err());
        // Absolute path that climbs back out.
        let outside = format!("{}/../escape", root.display());
        assert!(confine_within(Some(&root), &outside).is_err());
    }

    #[test]
    fn test_allows_harmless_internal_dotdot() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        // `a/../b` stays within the root.
        let resolved = confine_within(Some(&root), "a/../b").unwrap();
        assert!(resolved.starts_with(&root), "{resolved:?}");
        assert!(resolved.ends_with("b"), "{resolved:?}");
    }

    #[test]
    fn test_lexical_normalize_retains_escaping_dotdot() {
        // A leading `..` on a relative path is retained, not swallowed.
        assert_eq!(
            lexical_normalize(Path::new("../etc")),
            PathBuf::from("../etc")
        );
        // Over-pop keeps a residual `..`.
        assert_eq!(
            lexical_normalize(Path::new("a/../../b")),
            PathBuf::from("../b")
        );
    }
}
