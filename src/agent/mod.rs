// Agent module: Core agent loop and orchestration

mod context;

pub use context::Context;

use crate::provider::{CompletionRequest, Provider, ProviderError, ToolCall};
use crate::tools::ToolRegistry;
use std::sync::Arc;
use tracing::{debug, info, warn};

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
}

impl Agent {
    /// Create a new agent
    pub fn new(provider: Arc<dyn Provider>, tools: ToolRegistry) -> Self {
        Self {
            provider,
            tools,
            max_iterations: 50,
            system_prompt: Self::default_system_prompt(),
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

    /// Run a task
    pub async fn run(&self, task: Task) -> anyhow::Result<TaskResult> {
        info!("Starting task: {}", task.description);

        let mut context = Context::new();
        let max_iterations = task.max_iterations.unwrap_or(self.max_iterations);
        let mut tool_calls = Vec::new();

        // Add the task as the initial user message
        context.add_user_message(&task.description);

        for iteration in 0..max_iterations {
            debug!("Iteration {}", iteration + 1);

            // Build the completion request
            let request = self.build_request(&context)?;

            // Call the provider
            let response = self.provider.complete(request).await.map_err(|e| {
                match e {
                    ProviderError::ContextLengthExceeded => {
                        anyhow::anyhow!("Context length exceeded - task too complex")
                    }
                    _ => anyhow::anyhow!("Provider error: {}", e),
                }
            })?;

            // Handle the response
            if response.tool_calls.is_empty() {
                // No tool calls - task is complete
                info!("Task completed after {} iterations", iteration + 1);
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

                let result = self.execute_tool_call(tool_call.clone()).await;
                let (content, is_error) = match result {
                    Ok(r) => (r, false),
                    Err(e) => (format!("Error: {}", e), true),
                };

                tool_calls.push(ToolCallSummary {
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.to_string(),
                    result: content.clone(),
                    is_error,
                });

                context.add_tool_result(&tool_call.id, &content, is_error);
            }
        }

        // Max iterations reached
        warn!("Max iterations ({}) reached without completion", max_iterations);
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

    async fn execute_tool_call(&self, tool_call: ToolCall) -> anyhow::Result<String> {
        self.tools
            .execute(&tool_call.name, &tool_call.arguments)
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
- When you need to read files, use the file_read tool
- When you need to write files, use the file_write tool
- When you need to run commands, use the execute_command tool
- Always verify your changes work before declaring completion
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
