// File system tools

use crate::tools::workspace;
use crate::tools::{Result, Tool, ToolAnnotations, ToolError, ToolOutput};
use async_trait::async_trait;
use std::fs;
use tokio_util::sync::CancellationToken;

/// Run a blocking filesystem closure on the runtime's blocking pool so it
/// doesn't stall the async executor. Maps a task panic to a tool error.
async fn blocking<F>(f: F) -> Result<String>
where
    F: FnOnce() -> Result<String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("filesystem task failed: {e}")))?
}

/// Tool for reading file contents
pub struct FileReadTool;

#[async_trait]
impl Tool for FileReadTool {
    fn name(&self) -> &str {
        "file_read"
    }

    fn description(&self) -> &str {
        "Read a file's contents. Each line is prefixed with its 1-based line \
         number (the numbers are for display only and are not part of the file). \
         Optionally start at `offset` and read at most `limit` lines; at most \
         2000 lines are returned per call, so page through large files with \
         `offset`."
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "1-based line number to start reading from (default 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read (default and cap: 2000)"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "missing 'path' argument".to_string(),
            })?
            .to_string();
        // `as_u64` rejects negatives and non-integers; a 0 offset is clamped to
        // the first line in `format_with_line_numbers`.
        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        // `confine` (canonicalize) and the read are blocking; run them off the
        // async executor.
        blocking(move || {
            let path = workspace::confine(&path)?;
            let content = fs::read_to_string(&path)
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to read file: {e}")))?;
            Ok(format_with_line_numbers(&content, offset, limit))
        })
        .await
        .map(ToolOutput::from)
    }
}

/// Cap on the number of lines returned by a single `file_read`, so reading a
/// huge file can't blow up the context. Callers page past it with `offset`.
const MAX_READ_LINES: usize = 2000;

/// Render `content` as numbered lines (`<n>\t<line>`), starting at `offset`
/// (1-based, default 1) and returning at most `limit` lines (default and hard
/// cap [`MAX_READ_LINES`]). A trailing note flags any lines past the returned
/// window so the model knows to page with `offset`.
fn format_with_line_numbers(content: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    use std::fmt::Write as _;

    // Count lines without materializing a `Vec<&str>` the size of the whole file
    // just to return a small window.
    let total = content.lines().count();
    if total == 0 {
        return "(empty file)".to_string();
    }

    // 1-based start line, clamped to [1, total].
    let start = offset.unwrap_or(1).max(1);
    if start > total {
        return format!("(offset {start} is past the end of the file, which has {total} line(s))");
    }
    let start_idx = start - 1;
    let want = limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);
    let end_idx = start_idx.saturating_add(want).min(total);
    let shown = end_idx - start_idx;

    // Format directly into one buffer (write! avoids a temporary String per line);
    // rough pre-size of number + tab + line + newline.
    let mut out = String::with_capacity(shown * 40);
    for (i, line) in content.lines().skip(start_idx).take(shown).enumerate() {
        let _ = writeln!(out, "{:>6}\t{}", start + i, line);
    }
    if end_idx < total {
        let _ = write!(
            out,
            "... ({} more line(s); use offset={} to continue)",
            total - end_idx,
            end_idx + 1
        );
    }
    out.trim_end().to_string()
}

/// Tool for writing contents to a file
pub struct FileWriteTool;

#[async_trait]
impl Tool for FileWriteTool {
    fn name(&self) -> &str {
        "file_write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to write"
                },
                "content": {
                    "type": "string",
                    "description": "The content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "missing 'path' argument".to_string(),
            })?
            .to_string();
        let content = arguments
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "missing 'content' argument".to_string(),
            })?
            .to_string();

        blocking(move || {
            let confined = workspace::confine(&path)?;
            fs::write(&confined, &content)
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to write file: {e}")))?;
            Ok(format!(
                "Successfully wrote {} bytes to {}",
                content.len(),
                path
            ))
        })
        .await
        .map(ToolOutput::from)
    }
}

/// Tool for editing a file by replacing a unique occurrence of some text.
pub struct FileEditTool;

/// Why an edit could not be applied. Kept separate from the user-facing message
/// so the decision logic is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum EditError {
    /// `old` was empty (would match everywhere).
    EmptyOld,
    /// `old` and `new` are identical — nothing to do.
    Noop,
    /// `old` did not appear in the file.
    NotFound,
    /// `old` appeared more than once (the 1-based line of each occurrence).
    Ambiguous(Vec<usize>),
}

/// 1-based line number of the byte at `byte_idx` in `content`.
fn line_of_byte(content: &str, byte_idx: usize) -> usize {
    content[..byte_idx].bytes().filter(|&b| b == b'\n').count() + 1
}

/// 1-based line numbers of every (non-overlapping) occurrence of `needle`.
fn match_line_numbers(content: &str, needle: &str) -> Vec<usize> {
    // An empty needle matches at every position and never advances `start`, so
    // guard against it here as well as in the caller (avoids an infinite loop).
    if needle.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while let Some(pos) = content[start..].find(needle) {
        let abs = start + pos;
        lines.push(line_of_byte(content, abs));
        start = abs + needle.len();
    }
    lines
}

/// Compute the result of replacing the unique occurrence of `old` with `new`.
/// Returns the new file content and the 1-based line where the edit begins, or
/// an [`EditError`] explaining why it can't be applied uniquely.
fn compute_edit(
    content: &str,
    old: &str,
    new: &str,
) -> std::result::Result<(String, usize), EditError> {
    if old.is_empty() {
        return Err(EditError::EmptyOld);
    }
    if old == new {
        return Err(EditError::Noop);
    }
    let lines = match_line_numbers(content, old);
    match lines.as_slice() {
        [] => Err(EditError::NotFound),
        [line] => Ok((content.replacen(old, new, 1), *line)),
        _ => Err(EditError::Ambiguous(lines)),
    }
}

#[async_trait]
impl Tool for FileEditTool {
    fn name(&self) -> &str {
        "file_edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing an exact, unique occurrence of `old` with \
         `new`. `old` must match the file byte-for-byte (including whitespace and \
         indentation) and must appear exactly once — include enough surrounding \
         context to make it unique. Prefer this over file_write for changing part \
         of a file; use file_write only to create a file or replace it whole. \
         Note that file_read prefixes lines with display-only numbers; do not \
         include those in `old`."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the file to edit"
                },
                "old": {
                    "type": "string",
                    "description": "Exact text to find (must be unique in the file)"
                },
                "new": {
                    "type": "string",
                    "description": "Replacement text"
                }
            },
            "required": ["path", "old", "new"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let str_arg = |key: &str| -> Result<String> {
            arguments
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| ToolError::InvalidArguments {
                    name: "file_edit".to_string(),
                    reason: format!("missing '{key}' argument"),
                })
        };
        let path = str_arg("path")?;
        let old = str_arg("old")?;
        let new = str_arg("new")?;

        blocking(move || {
            let confined = workspace::confine(&path)?;
            let content = fs::read_to_string(&confined)
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to read file: {e}")))?;

            let (new_content, line) = compute_edit(&content, &old, &new).map_err(|e| match e {
                EditError::EmptyOld => ToolError::InvalidArguments {
                    name: "file_edit".to_string(),
                    reason: "`old` must not be empty".to_string(),
                },
                EditError::Noop => ToolError::InvalidArguments {
                    name: "file_edit".to_string(),
                    reason: "`old` and `new` are identical; nothing to change".to_string(),
                },
                EditError::NotFound => {
                    // A whitespace-only difference is the most common cause, so
                    // point at it when the trimmed text does appear.
                    let hint = if old.trim() != old && content.contains(old.trim()) {
                        " (a match exists ignoring surrounding whitespace — check \
                         indentation and trailing spaces)"
                    } else {
                        ""
                    };
                    ToolError::ExecutionFailed(format!(
                        "no exact occurrence of `old` was found in {path}{hint}"
                    ))
                }
                EditError::Ambiguous(lines) => {
                    let shown: Vec<String> = lines.iter().take(5).map(|l| l.to_string()).collect();
                    let more = if lines.len() > 5 { ", ..." } else { "" };
                    ToolError::ExecutionFailed(format!(
                        "`old` matches {} times in {path} (lines {}{}); add surrounding \
                         context so it matches exactly once",
                        lines.len(),
                        shown.join(", "),
                        more
                    ))
                }
            })?;

            // Write atomically: write a sibling temp file then rename over the
            // original, so a failure mid-write (disk full, crash) never leaves
            // the file truncated. The temp lives in the same directory so the
            // rename stays on one filesystem.
            let tmp = confined.with_file_name(format!(
                ".{}.boitata-{}.tmp",
                confined
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                uuid::Uuid::new_v4()
            ));
            fs::write(&tmp, &new_content).map_err(|e| {
                ToolError::ExecutionFailed(format!("failed to write temp file: {e}"))
            })?;
            // The temp file is created with default permissions; carry over the
            // original file's mode so an edit doesn't silently drop, say, the
            // executable bit. Best-effort — a failure here shouldn't abort a
            // successful edit.
            if let Ok(meta) = fs::metadata(&confined) {
                let _ = fs::set_permissions(&tmp, meta.permissions());
            }
            if let Err(e) = fs::rename(&tmp, &confined) {
                let _ = fs::remove_file(&tmp);
                return Err(ToolError::ExecutionFailed(format!(
                    "failed to replace file: {e}"
                )));
            }

            // Show the edited region (a few lines of context) so the caller can
            // verify the change landed where intended.
            let new_span = new.matches('\n').count() + 1;
            let snippet = format_with_line_numbers(
                &new_content,
                Some(line.saturating_sub(2).max(1)),
                Some(new_span + 4),
            );
            Ok(format!(
                "Edited {path}: replaced 1 occurrence at line {line}.\n{snippet}"
            ))
        })
        .await
        .map(ToolOutput::from)
    }
}

/// Tool for listing directory contents
pub struct ListDirectoryTool;

#[async_trait]
impl Tool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }

    fn description(&self) -> &str {
        "List the contents of a directory. Returns a list of file and directory names."
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations::read_only()
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The path to the directory to list"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        arguments: serde_json::Value,
        _cancel: CancellationToken,
    ) -> Result<ToolOutput> {
        let path = arguments
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments {
                name: self.name().to_string(),
                reason: "missing 'path' argument".to_string(),
            })?
            .to_string();

        blocking(move || {
            let path = workspace::confine(&path)?;
            let entries = fs::read_dir(&path).map_err(|e| {
                ToolError::ExecutionFailed(format!("failed to read directory: {e}"))
            })?;

            let mut result = Vec::new();
            for entry in entries {
                let entry = entry.map_err(|e| {
                    ToolError::ExecutionFailed(format!("failed to read directory entry: {e}"))
                })?;

                let name = entry.file_name().to_string_lossy().to_string();
                let metadata = entry.metadata().map_err(|e| {
                    ToolError::ExecutionFailed(format!("failed to read metadata: {e}"))
                })?;

                let file_type = if metadata.is_dir() {
                    "DIR"
                } else if metadata.is_file() {
                    "FILE"
                } else {
                    "OTHER"
                };

                result.push(format!("{file_type} {name}"));
            }

            Ok(result.join("\n"))
        })
        .await
        .map(ToolOutput::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_file_read() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");
        fs::write(&file_path, "Hello, World!").unwrap();

        let tool = FileReadTool;
        let result = tool
            .execute(
                serde_json::json!({"path": file_path.to_str()}),
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_ok());
        // The content is returned with a 1-based line-number prefix.
        assert_eq!(result.unwrap().to_text(), "     1\tHello, World!");
    }

    #[test]
    fn test_format_with_line_numbers_basic() {
        let out = format_with_line_numbers("a\nb\nc", None, None);
        assert_eq!(out, "     1\ta\n     2\tb\n     3\tc");
    }

    #[test]
    fn test_format_with_line_numbers_offset_and_limit() {
        // Start at line 2, take 2 lines; a 4th line remains, so a paging note
        // points at the next offset.
        let out = format_with_line_numbers("a\nb\nc\nd", Some(2), Some(2));
        assert_eq!(
            out,
            "     2\tb\n     3\tc\n... (1 more line(s); use offset=4 to continue)"
        );
    }

    #[test]
    fn test_format_with_line_numbers_offset_zero_clamps_to_one() {
        let out = format_with_line_numbers("only", Some(0), None);
        assert_eq!(out, "     1\tonly");
    }

    #[test]
    fn test_format_with_line_numbers_offset_past_end() {
        let out = format_with_line_numbers("a\nb", Some(5), None);
        assert!(out.contains("past the end"), "{out}");
    }

    #[test]
    fn test_format_with_line_numbers_empty() {
        assert_eq!(format_with_line_numbers("", None, None), "(empty file)");
    }

    #[test]
    fn test_format_with_line_numbers_caps_at_max() {
        // 3000 lines, no explicit limit: only MAX_READ_LINES are returned, plus a
        // trailing note pointing at the next page.
        let content: String = (1..=3000).map(|n| format!("line{n}\n")).collect();
        let out = format_with_line_numbers(&content, None, None);
        assert_eq!(out.lines().count(), MAX_READ_LINES + 1); // + trailing note
        assert!(
            out.contains(&format!("use offset={}", MAX_READ_LINES + 1)),
            "{out}"
        );
    }

    #[test]
    fn test_compute_edit_unique() {
        let (out, line) = compute_edit("a\nBBB\nc", "BBB", "X").unwrap();
        assert_eq!(out, "a\nX\nc");
        assert_eq!(line, 2);
    }

    #[test]
    fn test_compute_edit_not_found() {
        assert_eq!(compute_edit("a\nb", "zzz", "x"), Err(EditError::NotFound));
    }

    #[test]
    fn test_compute_edit_ambiguous_reports_lines() {
        assert_eq!(
            compute_edit("dup\nother\ndup", "dup", "x"),
            Err(EditError::Ambiguous(vec![1, 3]))
        );
    }

    #[test]
    fn test_compute_edit_empty_and_noop() {
        assert_eq!(compute_edit("a", "", "x"), Err(EditError::EmptyOld));
        assert_eq!(compute_edit("a", "a", "a"), Err(EditError::Noop));
    }

    #[test]
    fn test_match_line_numbers_empty_needle_is_empty() {
        // An empty needle must not loop forever; it returns no matches.
        assert!(match_line_numbers("abc\ndef", "").is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_file_edit_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("script.sh");
        fs::write(&file_path, "echo OLD\n").unwrap();
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o755)).unwrap();

        FileEditTool
            .execute(
                serde_json::json!({"path": file_path.to_str(), "old": "OLD", "new": "NEW"}),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // The executable bit survives the atomic write-then-rename.
        let mode = fs::metadata(&file_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "mode was {:o}", mode & 0o777);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "echo NEW\n");
    }

    #[tokio::test]
    async fn test_file_edit_applies_unique_change() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("edit.txt");
        fs::write(&file_path, "line1\nTARGET\nline3\n").unwrap();

        let result = FileEditTool
            .execute(
                serde_json::json!({
                    "path": file_path.to_str(),
                    "old": "TARGET",
                    "new": "REPLACED"
                }),
                CancellationToken::new(),
            )
            .await
            .unwrap()
            .to_text();

        assert!(result.contains("line 2"), "{result}");
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "line1\nREPLACED\nline3\n"
        );
    }

    #[tokio::test]
    async fn test_file_edit_rejects_ambiguous_match() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("edit.txt");
        fs::write(&file_path, "dup\ndup\n").unwrap();

        let err = FileEditTool
            .execute(
                serde_json::json!({"path": file_path.to_str(), "old": "dup", "new": "x"}),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 times"), "{err}");
        // The file is left untouched when the edit can't be applied.
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "dup\ndup\n");
    }

    #[tokio::test]
    async fn test_file_write() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        let tool = FileWriteTool;
        let result = tool
            .execute(
                serde_json::json!({
                    "path": file_path.to_str(),
                    "content": "Hello, World!"
                }),
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_ok());
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "Hello, World!");
    }

    #[tokio::test]
    async fn test_list_directory() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("file1.txt"), "test").unwrap();
        fs::write(temp_dir.path().join("file2.txt"), "test").unwrap();

        let tool = ListDirectoryTool;
        let result = tool
            .execute(
                serde_json::json!({"path": temp_dir.path().to_str()}),
                CancellationToken::new(),
            )
            .await;

        assert!(result.is_ok());
        let content = result.unwrap().to_text();
        assert!(content.contains("file1.txt"));
        assert!(content.contains("file2.txt"));
    }
}
