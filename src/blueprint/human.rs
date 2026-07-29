// Human-in-the-loop: how a blueprint asks a human operator for input mid-run.
//
// A `human` node (see [`super::nodes::HumanNode`]) pauses the run to collect a
// reply — an approval gate or a free-text answer — through the [`HumanInterface`]
// on the [`NodeCtx`](super::nodes::NodeCtx). The default [`StdioHuman`] prompts
// on the terminal; tests inject a scripted implementation. Because the reply is
// awaited inline, a blueprint that needs human input can't run unattended: a
// non-interactive stdin surfaces as a clear error rather than hanging.

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Collects a reply from a human operator during a blueprint run.
#[async_trait]
pub trait HumanInterface: Send + Sync {
    /// Present `prompt` and return the operator's reply (trimmed). Errors if no
    /// input is available (e.g. a non-interactive stdin at EOF) or the run is
    /// cancelled while waiting.
    async fn prompt(&self, prompt: &str, cancel: &CancellationToken) -> anyhow::Result<String>;
}

/// Default terminal implementation: writes the prompt to stderr (so it never
/// pollutes piped stdout) and reads one line from stdin.
///
/// The `BufReader` is created once and held for the whole run: a `read_line` may
/// buffer bytes past the newline, so a fresh reader per prompt would discard any
/// read-ahead and drop input when a blueprint has several `human` nodes.
pub struct StdioHuman {
    stdin: tokio::sync::Mutex<tokio::io::BufReader<tokio::io::Stdin>>,
}

impl Default for StdioHuman {
    fn default() -> Self {
        Self {
            stdin: tokio::sync::Mutex::new(tokio::io::BufReader::new(tokio::io::stdin())),
        }
    }
}

impl StdioHuman {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl HumanInterface for StdioHuman {
    async fn prompt(&self, prompt: &str, cancel: &CancellationToken) -> anyhow::Result<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let mut stderr = tokio::io::stderr();
        stderr
            .write_all(format!("\n{prompt}\n> ").as_bytes())
            .await?;
        stderr.flush().await?;

        let mut reader = self.stdin.lock().await;
        let mut line = String::new();
        let read = tokio::select! {
            _ = cancel.cancelled() => anyhow::bail!("human input cancelled"),
            result = reader.read_line(&mut line) => result?,
        };
        if read == 0 {
            anyhow::bail!("human input required but stdin reached EOF (non-interactive run?)");
        }
        Ok(line.trim().to_string())
    }
}
