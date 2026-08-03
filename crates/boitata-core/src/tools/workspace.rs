// Optional workspace-root confinement for path-taking tools.
//
// Boitata is a coding agent that operates on the user's files, so by default its
// tools can touch any path the process can (the containment story is deployment-
// level isolation — a container or devbox, per the project's Minions model).
//
// When a `workspace_root` is configured, path-taking tools (file_read/write,
// list_directory, search) confine their target to that root: absolute paths
// outside it, `..` traversal, and symlinks (in the existing path prefix) that
// escape it are rejected. This is applied consistently across all such tools
// rather than special-casing one.
//
// Known limitations — this is *lexical + canonicalize* confinement, not a
// syscall-level jail:
//   - TOCTOU: a directory component could be swapped for an escaping symlink
//     between the check here and the tool actually opening the path.
//   - Symlink tail: a component that doesn't exist yet is appended unchecked, so
//     a symlink created there afterward could redirect a later write outside the
//     root.
// Airtight containment would require opening each component with
// O_NOFOLLOW/openat2. This layer raises the bar against accidental and simple
// escapes; the real boundary remains deployment-level isolation (a container or
// devbox, per the project's Minions model).

use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use crate::tools::{Result, ToolError};

/// The configured workspace root, canonicalized. `None` (unset) means unconfined.
static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Set the workspace root once, at startup. Passing `None` leaves tools
/// unconfined. Calling more than once is a no-op (the first value wins); a
/// `static OnceLock` can't be reset, which is why tests exercise the pure
/// [`confine_within`] instead of the global `confine`/`init` pair.
pub fn init(root: Option<PathBuf>) {
    let resolved = root.map(|r| {
        // Canonicalize so later `starts_with` checks compare resolved (symlink-
        // free) paths. If the root doesn't exist yet, fall back to a lexically
        // absolute form — an *absolute* root is what keeps the containment check
        // sound, so never leave it relative.
        let abs = std::fs::canonicalize(&r).unwrap_or_else(|_| {
            tracing::warn!(
                "could not canonicalize workspace root `{}`; using its absolute lexical path (symlinks in the root won't be resolved)",
                r.display()
            );
            std::path::absolute(&r).unwrap_or(r)
        });
        if !abs.is_absolute() {
            // Should be unreachable (both branches above yield absolute paths);
            // if it somehow happens, `confine` fails closed rather than trusting
            // a relative root.
            tracing::error!(
                "workspace root `{}` is not absolute; path confinement will reject all paths",
                abs.display()
            );
        }
        abs
    });
    if ROOT.set(resolved).is_err() {
        tracing::warn!("workspace root already initialized; ignoring repeat init()");
    }
}

/// Confine `path` to the configured workspace root, returning the path to
/// actually use. Unconfined when no root is set. Errors if `path` resolves
/// outside the root.
///
/// Does a little blocking I/O (one `canonicalize` of the existing prefix). It's
/// cheap next to what the callers do: `exec::run_raw`/git/cargo immediately
/// spawn a subprocess, and `fs.rs` already runs its whole body (this call plus
/// the read/write) on `spawn_blocking`. So it is called directly rather than
/// forcing every caller onto an async signature for one stat.
pub fn confine(path: &str) -> Result<PathBuf> {
    let root = ROOT.get().and_then(|o| o.as_ref());
    confine_within(root.map(PathBuf::as_path), path)
}

/// Pure core of [`confine`], parameterized on the root for testability.
fn confine_within(root: Option<&Path>, path: &str) -> Result<PathBuf> {
    let Some(root) = root else {
        return Ok(PathBuf::from(path));
    };

    // Fail closed: a non-absolute root can't be reliably contained (see init).
    if !root.is_absolute() {
        return Err(ToolError::ExecutionFailed(
            "workspace root is not absolute; refusing to resolve path".to_string(),
        ));
    }

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

    // NOTE: TOCTOU race — between this check and the caller's open()/read()/write(),
    // a directory component inside the root could be replaced with a symlink
    // pointing outside it. `file_write` is the most exposed: it accepts a
    // not-yet-existing tail, so a symlink created there afterward could redirect
    // the write out of the root. This module relies on deployment-level isolation
    // (container/devbox) as the primary containment; callers should prefer
    // `O_NOFOLLOW` / `openat2(RESOLVE_BENEATH)` when available.
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
                // Unix-only assumption: at a filesystem root, `..` has nowhere to
                // go, so drop it. On Windows a bare drive `Prefix` (e.g. `C:`
                // without a following `RootDir`) is drive-*relative*, so dropping
                // a trailing `..` there would be wrong, but Windows isn't a target.
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
            // `ancestor` comes from `path.ancestors()`, so `strip_prefix` always
            // succeeds. Re-appending the (possibly non-existent) tail onto the
            // canonicalized prefix keeps a symlinked prefix resolved (e.g. macOS
            // `/tmp` -> `/private/tmp`) while still allowing not-yet-created files.
            let tail = path
                .strip_prefix(ancestor)
                .expect("ancestor is a prefix of path");
            // When the whole path already exists, `tail` is empty. `canonical.join("")`
            // appends a trailing separator (`<file>/`), which makes the kernel treat an
            // existing regular file as "open as directory" → ENOTDIR on the caller's
            // subsequent read/open (e.g. `file_read`/`file_edit`). Return the canonical
            // path as-is in that case.
            if tail.as_os_str().is_empty() {
                return canonical;
            }
            return canonical.join(tail);
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
        // The existing file must resolve to exactly root/a.txt — no trailing
        // separator. A trailing slash (`a.txt/`) turns the subsequent open into
        // "open file as directory" → ENOTDIR, breaking file_read/file_edit.
        assert_eq!(resolved, root.join("a.txt"));
        assert_eq!(
            std::fs::read_to_string(&resolved).unwrap(),
            "x",
            "resolved path must be readable"
        );

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
