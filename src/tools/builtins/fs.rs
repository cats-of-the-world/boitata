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
        "Read the contents of a file. Returns the file contents as a string."
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

        // `confine` (canonicalize) and the read are blocking; run them off the
        // async executor.
        blocking(move || {
            let path = workspace::confine(&path)?;
            fs::read_to_string(&path)
                .map_err(|e| ToolError::ExecutionFailed(format!("failed to read file: {e}")))
        })
        .await
        .map(ToolOutput::from)
    }
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
        assert_eq!(result.unwrap().to_text(), "Hello, World!");
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
