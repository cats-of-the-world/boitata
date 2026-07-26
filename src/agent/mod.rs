// Agent module: Core agent loop and orchestration

use crate::audit::{AuditEvent, AuditSink};
use crate::context::{
    Context, KEEP_RECENT_MESSAGES, SUMMARIZATION_SYSTEM_PROMPT, TokenCounter, apply_summary,
    needs_compaction, pick_cutoff, render_for_summary,
};
use crate::provider::{
    CompletionRequest, Message, MessageContent, MessageRole, Provider, ProviderError, ToolCall,
    ToolDefinition,
};
use crate::tools::{Decision, ToolOutput, ToolPolicy, ToolRegistry};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Error string recorded when a run is stopped by cancellation (Ctrl-C).
const CANCELLED_ERROR: &str = "Cancelled";

/// Default fraction of the model's context window at which older turns are
/// summarized. Matches goose's default auto-compaction threshold.
const DEFAULT_COMPACT_THRESHOLD: f32 = 0.8;

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
    policy: ToolPolicy,
    /// Fraction of the model's context window at which older turns are summarized.
    /// `0.0` disables compaction.
    compact_threshold: f32,
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
            // Fully permissive by default, preserving prior behavior.
            policy: ToolPolicy::allow_all(),
            compact_threshold: DEFAULT_COMPACT_THRESHOLD,
        }
    }

    /// Set the tool permission policy consulted before each tool call.
    pub fn with_policy(mut self, policy: ToolPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Set the fraction of the model's context window at which older turns are
    /// summarized into a synopsis. `0.0` disables compaction.
    ///
    /// The value is clamped to `[0.0, 1.0]`: a fraction above 1.0 could never be
    /// reached (so compaction would never fire), and `NaN` would slip past the
    /// comparison guards and silently disable compaction — both are treated as
    /// misconfiguration and corrected.
    pub fn with_compact_threshold(mut self, threshold: f32) -> Self {
        self.compact_threshold = if threshold.is_nan() {
            DEFAULT_COMPACT_THRESHOLD
        } else {
            threshold.clamp(0.0, 1.0)
        };
        self
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

        // Tokenizer and tool definitions are fixed for the run; build them once and
        // reuse them every iteration (for both the request and compaction checks).
        // The tool-schema token count is likewise constant, so compute it once.
        let counter = TokenCounter::new();
        let tool_defs = if self.provider.supports_tools() {
            self.tools.to_definitions()
        } else {
            Vec::new()
        };
        let tool_tokens = counter.count_tokens_for_tools(&tool_defs);

        // Record the system prompt on the context so token accounting reflects it
        // (it is a fixed, non-trivial share of every request).
        context.set_system_prompt(self.system_prompt.clone());

        // Add the task as the initial user message
        context.add_user_message(&task.description);

        self.emit(AuditEvent::RunStarted {
            task: task.description.clone(),
            provider: self.provider.name().to_string(),
            model: self.provider.model().to_string(),
        });

        for iteration in 0..max_iterations {
            debug!("Iteration {}", iteration + 1);

            // Summarize older turns if we're approaching the context window, so a
            // long run compacts instead of overflowing. Best-effort: on failure we
            // proceed and let the provider's own limit be the backstop.
            self.maybe_compact(&mut context, &counter, tool_tokens, iteration, &cancel)
                .await;

            // Build the completion request, reusing the tool definitions computed
            // once above rather than rebuilding them each iteration.
            let request = self.build_request(&context, &tool_defs)?;

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

                // Consult the permission policy before running the tool. A denial
                // is reported to the model as an error result (so it can adapt)
                // and the tool never runs.
                let annotations = self.tools.annotations(&tool_call.name);
                let (output, is_error, denied) =
                    match self
                        .policy
                        .decide(&tool_call.name, annotations, &tool_call.arguments)
                    {
                        Decision::Deny(reason) => {
                            warn!("Tool `{}` denied by policy: {reason}", tool_call.name);
                            // Build the model-facing message first, then move
                            // `reason` into the audit event (no clone needed).
                            let output =
                                ToolOutput::text(format!("Denied by tool policy: {reason}"));
                            self.emit(AuditEvent::ToolDenied {
                                iteration: iteration + 1,
                                name: tool_call.name.clone(),
                                arguments: tool_call.arguments.to_string(),
                                reason,
                            });
                            (output, true, true)
                        }
                        Decision::Allow => {
                            match self
                                .execute_tool_call(tool_call.clone(), cancel.clone())
                                .await
                            {
                                Ok(output) => (output, false, false),
                                Err(e) => (ToolOutput::text(format!("Error: {e}")), true, false),
                            }
                        }
                    };
                // Flatten to text for the text-only sinks (audit log + CLI
                // summary); the structured content is carried into the context.
                let text = output.to_text();
                // Reuse the annotations already fetched for the policy decision.
                let read_only = annotations.map(|a| a.read_only).unwrap_or(false);

                // A denied call was never executed and is already captured by the
                // `ToolDenied` event; don't also emit a `ToolCall` (which would
                // imply the tool ran and returned an error).
                if !denied {
                    self.emit(AuditEvent::ToolCall {
                        iteration: iteration + 1,
                        name: tool_call.name.clone(),
                        arguments: tool_call.arguments.to_string(),
                        result: text.clone(),
                        is_error,
                        read_only,
                    });
                }

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

    fn build_request(
        &self,
        context: &Context,
        tool_defs: &[ToolDefinition],
    ) -> Result<CompletionRequest, ProviderError> {
        let messages = context.to_messages();

        // Reuse the precomputed tool definitions (`tool_defs` is empty when the
        // provider doesn't support tools).
        let tools = if self.provider.supports_tools() {
            Some(tool_defs.to_vec())
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

    /// Summarize the oldest turns when the context nears the model's window.
    /// Best-effort: any failure — no suitable cutoff, cancellation, a summarizer
    /// error, or an empty summary — leaves the context untouched, and the
    /// provider's own limit remains the backstop.
    async fn maybe_compact(
        &self,
        context: &mut Context,
        counter: &TokenCounter,
        tool_tokens: usize,
        iteration: usize,
        cancel: &CancellationToken,
    ) {
        if self.compact_threshold <= 0.0 {
            return;
        }
        let used = context.token_count(counter, tool_tokens);
        if !needs_compaction(used, self.provider.context_limit(), self.compact_threshold) {
            return;
        }
        let Some(cutoff) = pick_cutoff(context, KEEP_RECENT_MESSAGES) else {
            return;
        };

        let messages_before = context.len();
        let to_summarize = render_for_summary(context, cutoff);

        // Ask the model to summarize the older turns. A lower temperature keeps
        // the synopsis faithful. Race against cancellation like the main call.
        let request = CompletionRequest {
            messages: vec![Message {
                role: MessageRole::User,
                content: MessageContent::Text(to_summarize),
            }],
            tools: None,
            max_tokens: Some(self.provider.max_tokens()),
            temperature: Some(0.3),
            system: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        };

        let completion = tokio::select! {
            biased;
            _ = cancel.cancelled() => return,
            completion = self.provider.complete(request) => completion,
        };

        let summary = match completion {
            Ok(response) => response.content.unwrap_or_default(),
            Err(e) => {
                warn!("Context compaction summarization failed: {e}");
                return;
            }
        };
        if summary.trim().is_empty() {
            warn!("Context compaction produced an empty summary; skipping");
            return;
        }

        apply_summary(context, cutoff, summary);
        let tokens_after = context.token_count(counter, tool_tokens);
        info!(
            "Compacted context: {messages_before} -> {} messages (~{used} -> ~{tokens_after} tokens)",
            context.len()
        );
        self.emit(AuditEvent::ContextCompacted {
            iteration: iteration + 1,
            tokens_before: used,
            tokens_after,
            messages_before,
            messages_after: context.len(),
        });
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

    // A provider that first drives several tool-calling turns (to grow the
    // history) and then finishes, returning a canned synopsis whenever the agent
    // asks it to summarize. Its context window is tiny so the compaction
    // threshold is crossed as soon as there are enough turns to summarize.
    struct CompactingProvider {
        main_calls: std::sync::atomic::AtomicUsize,
        summary_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for CompactingProvider {
        fn name(&self) -> &str {
            "fake"
        }
        fn model(&self) -> &str {
            "fake-model"
        }
        fn context_limit(&self) -> usize {
            50
        }
        fn max_tokens(&self) -> usize {
            128
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> crate::provider::ProviderResult<crate::provider::CompletionResponse> {
            use std::sync::atomic::Ordering;

            // The summarization call is the one carrying the summarization system
            // prompt and no tools.
            if request.system.as_deref() == Some(SUMMARIZATION_SYSTEM_PROMPT) {
                self.summary_calls.fetch_add(1, Ordering::SeqCst);
                return Ok(crate::provider::CompletionResponse {
                    content: Some("compact synopsis".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                });
            }

            let n = self.main_calls.fetch_add(1, Ordering::SeqCst);
            if n < 5 {
                // Grow the history with tool calls. The tool is unregistered, so
                // it yields error results — which still enlarge the context.
                Ok(crate::provider::CompletionResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: format!("call-{n}"),
                        name: "noop".to_string(),
                        arguments: serde_json::json!({ "i": n }),
                    }],
                    usage: None,
                    finish_reason: Some("tool_use".to_string()),
                })
            } else {
                Ok(crate::provider::CompletionResponse {
                    content: Some("all done".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                    finish_reason: Some("stop".to_string()),
                })
            }
        }

        async fn stream_complete(
            &self,
            _request: CompletionRequest,
        ) -> crate::provider::ProviderResult<
            tokio_stream::wrappers::ReceiverStream<
                crate::provider::ProviderResult<crate::provider::Chunk>,
            >,
        > {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(tokio_stream::wrappers::ReceiverStream::new(rx))
        }
    }

    #[tokio::test]
    async fn compaction_fires_and_run_completes() {
        use std::sync::atomic::Ordering;

        let provider = Arc::new(CompactingProvider {
            main_calls: std::sync::atomic::AtomicUsize::new(0),
            summary_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = Agent::new(provider.clone(), ToolRegistry::new());
        let result = agent
            .run(Task::new("do a big task".to_string()))
            .await
            .expect("run should not error");

        assert!(result.success, "run should complete: {result:?}");
        assert!(
            provider.summary_calls.load(Ordering::SeqCst) >= 1,
            "compaction should have summarized the older turns at least once"
        );
    }

    #[tokio::test]
    async fn disabled_threshold_never_compacts() {
        use std::sync::atomic::Ordering;

        let provider = Arc::new(CompactingProvider {
            main_calls: std::sync::atomic::AtomicUsize::new(0),
            summary_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let agent = Agent::new(provider.clone(), ToolRegistry::new()).with_compact_threshold(0.0);
        let result = agent
            .run(Task::new("do a big task".to_string()))
            .await
            .expect("run should not error");

        assert!(result.success);
        assert_eq!(
            provider.summary_calls.load(Ordering::SeqCst),
            0,
            "compaction must not run when the threshold is 0"
        );
    }
}
