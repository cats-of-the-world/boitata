// Agent module: Core agent loop and orchestration

mod context;

pub use context::Context;

use crate::audit::{AuditEvent, AuditSink};
use crate::provider::{CompletionRequest, Provider, ProviderError, ToolCall};
use crate::tools::{ToolOutput, ToolRegistry};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Error string recorded when a run is stopped by cancellation (Ctrl-C).
const CANCELLED_ERROR: &str = "Cancelled";

/// A task to be executed by the agent
#[derive(Debug, Clone)]
pub struct Task {
    pub description: String,
    pub workspace: Option<String>,
    pub max_iterations: Option<usize>,
}

impl Task {
    pub fn new(description: String) -> Self {
        Self {
            description,
            workspace: None,
            max_iterations: None,
        }
    }

    pub fn with_workspace(mut self, workspace: String) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = Some(max_iterations);
        self
    }
}

/// Result from an agent run
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub success: bool,
    pub final_message: Option<String>,
    pub iterations: usize,
    pub tool_calls: Vec<ToolCallSummary>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCallSummary {
    pub name: String,
    pub arguments: String,
    pub result: String,
    pub is_error: bool,
}

/// The core agent
pub struct Agent {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    max_iterations: usize,
    system_prompt: String,
    audit: Option<Arc<dyn AuditSink>>,
}

impl Agent {
    /// Create a new agent
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self {
        Self {
            provider,
            tools,
            max_iterations: 50,
            system_prompt: Self::default_system_prompt(),
            audit: None,
        }
    }

    /// Set a custom system prompt
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Set the maximum iterations
    pub fn with_max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Attach an audit sink that records run events (start, LLM responses, tool
    /// calls, completion).
    pub fn with_audit(mut self, audit: Arc<dyn AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Emit an audit event if a sink is attached. Best-effort — never fails.
    fn emit(&self, event: AuditEvent) {
        if let Some(audit) = &self.audit {
            audit.record(event);
        }
    }

    /// Record and build the result for a run cut short by cancellation. Shared by
    /// the "cancelled while awaiting the model" and "cancelled mid-tool-batch"
    /// paths so the two stay in lockstep.
    fn cancelled_result(
        &self,
        iterations: usize,
        tool_calls: Vec<ToolCallSummary>,
        total_input_tokens: usize,
        total_output_tokens: usize,
    ) -> TaskResult {
        self.emit(AuditEvent::RunCompleted {
            success: false,
            iterations,
            error: Some(CANCELLED_ERROR.to_string()),
            total_input_tokens,
            total_output_tokens,
        });
        TaskResult {
            success: false,
            final_message: None,
            iterations,
            tool_calls,
            error: Some(CANCELLED_ERROR.to_string()),
        }
    }

    /// Run a task. Interrupts (Ctrl-C) cancel the in-flight tool and stop the
    /// run; the running tool's subprocess/remote call is torn down promptly.
    pub async fn run(&self, task: Task) -> anyhow::Result<TaskResult> {
        let cancel = CancellationToken::new();
        // Cancel the run on Ctrl-C. The watcher is aborted once the run returns
        // so it doesn't linger between runs.
        let watcher = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    info!("Interrupt received; cancelling run");
                    cancel.cancel();
                }
            })
        };
        let result = self.run_with_cancel(task, cancel).await;
        watcher.abort();
        result
    }

    /// Run a task under an external [`CancellationToken`]. Exposed for callers
    /// (and tests) that want to drive cancellation directly; [`Agent::run`]
    /// wraps this with a Ctrl-C watcher.
    pub async fn run_with_cancel(
        &self,
        task: Task,
        cancel: CancellationToken,
    ) -> anyhow::Result<TaskResult> {
        info!("Starting task: {}", task.description);

        let mut context = Context::new();
        let max_iterations = task.max_iterations.unwrap_or(self.max_iterations);
        let mut tool_calls = Vec::new();
        let mut total_input_tokens = 0usize;
        let mut total_output_tokens = 0usize;

        // Add the task as the initial user message
        context.add_user_message(&task.description);

        self.emit(AuditEvent::RunStarted {
            task: task.description.clone(),
            provider: self.provider.name().to_string(),
            model: self.provider.model().to_string(),
        });

        for iteration in 0..max_iterations {
            debug!("Iteration {}", iteration + 1);

            // Build the completion request
            let request = self.build_request(&context)?;

            // Call the provider, racing it against cancellation so Ctrl-C during
            // the (often multi-second) LLM call stops the run promptly instead of
            // waiting for the response. On failure, record it before propagating
            // so the audit log captures why an unattended run died (e.g. auth).
            let completion = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    warn!("Run cancelled while awaiting the model");
                    return Ok(self.cancelled_result(
                        iteration + 1,
                        tool_calls,
                        total_input_tokens,
                        total_output_tokens,
                    ));
                }
                completion = self.provider.complete(request) => completion,
            };
            let response = match completion {
                Ok(response) => response,
                Err(e) => {
                    let message = match e {
                        ProviderError::ContextLengthExceeded => {
                            "Context length exceeded - task too complex".to_string()
                        }
                        other => format!("Provider error: {}", other),
                    };
                    self.emit(AuditEvent::RunCompleted {
                        success: false,
                        iterations: iteration + 1,
                        error: Some(message.clone()),
                        total_input_tokens,
                        total_output_tokens,
                    });
                    return Err(anyhow::anyhow!(message));
                }
            };

            // Track token usage for the audit trail.
            let (input_tokens, output_tokens) = match &response.usage {
                Some(usage) => {
                    total_input_tokens += usage.input_tokens;
                    total_output_tokens += usage.output_tokens;
                    (Some(usage.input_tokens), Some(usage.output_tokens))
                }
                None => (None, None),
            };

            self.emit(AuditEvent::LlmResponse {
                iteration: iteration + 1,
                has_text: response
                    .content
                    .as_ref()
                    .map(|c| !c.is_empty())
                    .unwrap_or(false),
                tool_calls: response.tool_calls.iter().map(|t| t.name.clone()).collect(),
                input_tokens,
                output_tokens,
            });

            // Handle the response
            if response.tool_calls.is_empty() {
                // No tool calls - task is complete
                info!("Task completed after {} iterations", iteration + 1);
                self.emit(AuditEvent::RunCompleted {
                    success: true,
                    iterations: iteration + 1,
                    error: None,
                    total_input_tokens,
                    total_output_tokens,
                });
                return Ok(TaskResult {
                    success: true,
                    final_message: response.content,
                    iterations: iteration + 1,
                    tool_calls,
                    error: None,
                });
            }

            // Record the assistant turn (text + tool_use blocks) before the tool
            // results, so each tool_result references a matching tool_use.
            context.add_assistant_tool_use(response.content.clone(), response.tool_calls.clone());

            // Execute tool calls
            for tool_call in &response.tool_calls {
                debug!("Executing tool: {}", tool_call.name);

                let result = self
                    .execute_tool_call(tool_call.clone(), cancel.clone())
                    .await;
                let (output, is_error) = match result {
                    Ok(output) => (output, false),
                    Err(e) => (ToolOutput::text(format!("Error: {e}")), true),
                };
                // Flatten to text for the text-only sinks (audit log + CLI
                // summary); the structured content is carried into the context.
                let text = output.to_text();
                let read_only = self
                    .tools
                    .annotations(&tool_call.name)
                    .map(|a| a.read_only)
                    .unwrap_or(false);

                self.emit(AuditEvent::ToolCall {
                    iteration: iteration + 1,
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.to_string(),
                    result: text.clone(),
                    is_error,
                    read_only,
                });

                tool_calls.push(ToolCallSummary {
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.to_string(),
                    result: text,
                    is_error,
                });

                context.add_tool_result(&tool_call.id, output.content, is_error);

                // Don't start the remaining tools in this batch if we were
                // cancelled mid-batch; the post-loop check reports the outcome.
                if cancel.is_cancelled() {
                    break;
                }
            }

            // Stop promptly if the run was cancelled while executing this
            // iteration's tools (the running tool already returned an error).
            if cancel.is_cancelled() {
                warn!("Run cancelled after {} iteration(s)", iteration + 1);
                return Ok(self.cancelled_result(
                    iteration + 1,
                    tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                ));
            }
        }

        // Max iterations reached
        warn!(
            "Max iterations ({}) reached without completion",
            max_iterations
        );
        self.emit(AuditEvent::RunCompleted {
            success: false,
            iterations: max_iterations,
            error: Some("Max iterations reached".to_string()),
            total_input_tokens,
            total_output_tokens,
        });
        Ok(TaskResult {
            success: false,
            final_message: None,
            iterations: max_iterations,
            tool_calls,
            error: Some("Max iterations reached".to_string()),
        })
    }

    fn build_request(&self, context: &Context) -> Result<CompletionRequest, ProviderError> {
        let messages = context.to_messages();

        // Add tool definitions if the provider supports them
        let tools = if self.provider.supports_tools() {
            Some(self.tools.to_definitions())
        } else {
            None
        };

        Ok(CompletionRequest {
            messages,
            tools,
            max_tokens: Some(self.provider.max_tokens()),
            temperature: Some(0.7),
            system: Some(self.system_prompt.clone()),
        })
    }

    async fn execute_tool_call(
        &self,
        tool_call: ToolCall,
        cancel: CancellationToken,
    ) -> anyhow::Result<ToolOutput> {
        self.tools
            .execute(&tool_call.name, &tool_call.arguments, cancel)
            .await
            .map_err(|e| anyhow::anyhow!("Tool execution error: {}", e))
    }

    fn default_system_prompt() -> String {
        r#"You are Boitata, a coding agent designed to help developers complete tasks efficiently.

Your role:
- Read and understand code
- Execute tools to modify files, run commands, and gather information
- Complete the task you've been given

Guidelines:
- Be concise and direct
- Read files with file_read; write them with file_write; find code with search
- Prefer the dedicated deterministic tools over execute_command: use cargo_check,
  cargo_clippy, cargo_fmt, cargo_test, and cargo_add for Rust work, and git_status,
  git_diff, git_commit, and git_branch for version control
- Fall back to execute_command only for operations without a dedicated tool
- Always verify your changes work (e.g. cargo_check / cargo_test) before finishing
- If you make a mistake, acknowledge it and fix it

The task is complete when you have:
1. Made the requested changes
2. Verified they work (ran tests if applicable)
3. Have no more tool calls to make

When finished, provide a brief summary of what you did."#
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_creation() {
        let task = Task::new("Fix the bug".to_string());
        assert_eq!(task.description, "Fix the bug");
        assert!(task.workspace.is_none());
        assert!(task.max_iterations.is_none());
    }

    #[test]
    fn test_task_with_options() {
        let task = Task::new("Fix the bug".to_string())
            .with_workspace("/tmp/test".to_string())
            .with_max_iterations(100);
        assert_eq!(task.description, "Fix the bug");
        assert_eq!(task.workspace, Some("/tmp/test".to_string()));
        assert_eq!(task.max_iterations, Some(100));
    }
}
