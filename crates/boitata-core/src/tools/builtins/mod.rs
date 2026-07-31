// Built-in tools

mod exec;

// Shell-script execution for blueprint script nodes (returns the exit code).
pub use exec::run_script;

pub mod cargo;
pub mod command;
pub mod fs;
pub mod git;
pub mod search;

// Re-export all built-in tools
pub use cargo::{CargoAddTool, CargoCheckTool, CargoClippyTool, CargoFmtTool, CargoTestTool};
pub use command::ExecuteCommandTool;
pub use fs::{FileEditTool, FileReadTool, FileWriteTool, ListDirectoryTool};
pub use git::{GitBranchTool, GitCommitTool, GitDiffTool, GitStatusTool};
pub use search::SearchTool;
