//! Sandbox-provisioning nodes.
//!
//! These nodes let a blueprint move execution off the host into an isolated
//! sandbox: [`ProvisionNode`] creates one, [`CheckoutNode`] git-clones a repo into
//! it, and [`ExecNode`] runs commands inside it. The sandbox backend (Docker
//! today) lives behind the [`Sandbox`](super::sandbox::Sandbox) trait; every
//! sandbox a run provisions is tracked by [`Sandboxes`](super::sandbox::Sandboxes)
//! and destroyed by the executor when the run ends — see
//! `Executor::run_with_cancel`.

use async_trait::async_trait;

use super::nodes::{Node, NodeCtx};
use super::state::{State, Status, Update, render};
use boitata_core::audit::NodeKind;

/// Where a checkout clones by default (inside the sandbox).
const DEFAULT_WORKSPACE: &str = "/workspace";

/// Build the shell command a [`CheckoutNode`] runs: clone `repo` into `path`, then
/// (optionally) check out `git_ref`. Kept pure so it can be unit-tested without a
/// daemon. Values are passed as `sh -c` arguments with the parts quoted.
fn checkout_command(repo: &str, path: &str, git_ref: Option<&str>) -> Vec<String> {
    let mut script = format!("git clone {} {}", shell_quote(repo), shell_quote(path));
    if let Some(git_ref) = git_ref {
        script.push_str(&format!(
            " && git -C {} checkout {}",
            shell_quote(path),
            shell_quote(git_ref)
        ));
    }
    vec!["sh".into(), "-c".into(), script]
}

/// Single-quote a value for safe inclusion in a `sh -c` script.
fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Create an ephemeral container from `image`. Its output (the container id) is
/// stored under the node name, so downstream nodes reference it as `{name}`.
pub struct ProvisionNode {
    name: String,
    image: String,
}

impl ProvisionNode {
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
        }
    }
}

#[async_trait]
impl Node for ProvisionNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Container
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let image = render(&self.image, state);
        let id = cx.sandbox.provision(&image, &cx.cancel).await?;
        Ok(Update::from_node(&self.name, id, Status::Ok))
    }
}

/// Git-clone `repo` into a container (referenced by `container`, e.g. `{box}`).
/// Routes on the clone's exit code.
pub struct CheckoutNode {
    name: String,
    container: String,
    repo: String,
    git_ref: Option<String>,
    path: String,
}

impl CheckoutNode {
    pub fn new(
        name: impl Into<String>,
        container: impl Into<String>,
        repo: impl Into<String>,
        git_ref: Option<String>,
        path: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            container: container.into(),
            repo: repo.into(),
            git_ref,
            path: path.unwrap_or_else(|| DEFAULT_WORKSPACE.to_string()),
        }
    }
}

#[async_trait]
impl Node for CheckoutNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Container
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let container = render(&self.container, state);
        let repo = render(&self.repo, state);
        let git_ref = self.git_ref.as_ref().map(|r| render(r, state));
        let argv = checkout_command(&repo, &self.path, git_ref.as_deref());
        let (code, output) = cx.sandbox.exec(&container, argv, None, &cx.cancel).await?;
        let status = if code == 0 {
            Status::Ok
        } else {
            Status::Failed
        };
        Ok(Update::from_node(&self.name, output, status))
    }
}

/// Run a shell command inside a container (referenced by `container`). Routes on
/// the command's exit code, mirroring `ScriptNode` but in the container.
pub struct ExecNode {
    name: String,
    container: String,
    run: String,
    workdir: Option<String>,
}

impl ExecNode {
    pub fn new(
        name: impl Into<String>,
        container: impl Into<String>,
        run: impl Into<String>,
        workdir: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            container: container.into(),
            run: run.into(),
            workdir,
        }
    }
}

#[async_trait]
impl Node for ExecNode {
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> NodeKind {
        NodeKind::Container
    }

    async fn run(&self, state: &State, cx: &NodeCtx<'_>) -> anyhow::Result<Update> {
        let container = render(&self.container, state);
        let command = render(&self.run, state);
        let argv = vec!["sh".to_string(), "-c".to_string(), command];
        let (code, output) = cx
            .sandbox
            .exec(&container, argv, self.workdir.as_deref(), &cx.cancel)
            .await?;
        let status = if code == 0 {
            Status::Ok
        } else {
            Status::Failed
        };
        Ok(Update::from_node(&self.name, output, status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkout_command_clones_into_path() {
        let argv = checkout_command("https://example.com/r.git", "/workspace", None);
        assert_eq!(argv[0], "sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(
            argv[2],
            "git clone 'https://example.com/r.git' '/workspace'"
        );
    }

    #[test]
    fn checkout_command_checks_out_ref() {
        let argv = checkout_command("r", "/w", Some("v1.2"));
        assert_eq!(argv[2], "git clone 'r' '/w' && git -C '/w' checkout 'v1.2'");
    }

    #[test]
    fn shell_quote_neutralizes_injection() {
        // A repo/ref containing shell metacharacters is quoted, not interpreted.
        let argv = checkout_command("; rm -rf /", "/w", None);
        assert_eq!(argv[2], "git clone '; rm -rf /' '/w'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
