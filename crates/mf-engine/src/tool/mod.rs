//! Tools, and the single gate every one of them passes through.
//!
//! The permission check lives in [`ToolRegistry::dispatch`] and nowhere else. Tools are never
//! invoked directly, so a tool that forgets to check its own permissions cannot exist — the
//! failure mode simply isn't reachable. Individual tools implement only their behaviour.

pub mod builtins;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    fsaccess::{Denied, PathPolicy},
    proto::ToolCallId,
    provider::ToolSchema,
};

bitflags! {
    /// What an agent is allowed to do. Stored as an integer on the agent row.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Permission: u32 {
        const READ  = 1 << 0;
        const WRITE = 1 << 1;
        const EXEC  = 1 << 2;
        const WEB   = 1 << 3;
        const AGENT = 1 << 4;
    }
}

impl Permission {
    /// The spec's role presets.
    pub const RESEARCH: Self = Self::READ.union(Self::WEB);
    pub const CODING: Self = Self::READ.union(Self::WRITE).union(Self::EXEC);
    pub const DOCUMENTATION: Self = Self::READ.union(Self::WRITE);
}

/// Everything a tool is allowed to touch, assembled per run.
pub struct ToolCtx {
    pub policy: PathPolicy,
    pub cwd: PathBuf,
    /// Guards `delegate` recursion. Hard cap enforced by the registry.
    pub depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Fed back to the model as the tool result.
    pub content: String,
    /// Shown in the UI's log; may be shorter than `content`.
    pub summary: String,
}

impl ToolOutput {
    pub fn new(content: impl Into<String>) -> Self {
        let content = content.into();
        let summary = content.lines().next().unwrap_or_default().chars().take(120).collect();
        Self { content, summary }
    }

    pub fn with_summary(content: impl Into<String>, summary: impl Into<String>) -> Self {
        Self { content: content.into(), summary: summary.into() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("permission denied: {tool} requires {needs:?}, agent has {has:?}")]
    Permission { tool: String, needs: Permission, has: Permission },
    #[error(transparent)]
    Path(#[from] Denied),
    #[error("no such tool: {0}")]
    Unknown(String),
    #[error("bad arguments for {tool}: {reason}")]
    BadArguments { tool: String, reason: String },
    #[error("user denied the request")]
    UserDenied,
    #[error("delegation nested too deeply (limit {limit})")]
    TooDeep { limit: u8 },
    #[error("{0}")]
    Failed(String),
}

impl ToolError {
    /// Whether the model should be told and allowed to try something else, as opposed to the run
    /// being abandoned. Nearly everything is recoverable: a denied path is information.
    pub fn is_recoverable(&self) -> bool {
        !matches!(self, ToolError::TooDeep { .. })
    }
}

/// What the user is shown before approving a call. Built from the arguments *before* execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPreview {
    pub title: String,
    /// The exact operation, rendered verbatim — a command line, or a diff.
    pub detail: String,
    /// One line, for the audit row and the log. `detail` can be a 400-line diff; storing that on
    /// every write would bloat the database without making the record more useful.
    pub summary: String,
    pub kind: PreviewKind,
}

/// How [`ToolPreview::detail`] should be rendered.
///
/// An explicit tag rather than having the UI sniff for leading `+`/`-`: a command line that
/// happens to start with a dash is not a diff, and guessing wrong on a consent screen means
/// colouring an argument as a deletion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewKind {
    #[default]
    Text,
    /// Unified diff: `+`, `-` and ` ` line prefixes.
    Diff,
}

impl ToolPreview {
    /// A plain-text preview, summarised by its first line.
    pub fn text(title: impl Into<String>, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let summary = detail.lines().next().unwrap_or_default().chars().take(200).collect();
        Self { title: title.into(), detail, summary, kind: PreviewKind::Text }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the arguments, shipped verbatim to the provider.
    fn parameters(&self) -> Value;
    fn permission(&self) -> Permission;

    /// Whether this call needs explicit user approval. Anything destructive says yes.
    fn needs_approval(&self, _args: &Value) -> bool {
        false
    }

    /// Rendered for the approval modal. Only called when `needs_approval` is true.
    ///
    /// Takes `ctx` and is async because an honest preview often has to look at the world: a write
    /// cannot show what it changes without reading the file it is about to replace.
    fn preview(&self, _ctx: &ToolCtx, args: &Value) -> impl Future<Output = ToolPreview> + Send {
        let preview = ToolPreview {
            title: self.name().to_owned(),
            detail: serde_json::to_string_pretty(args).unwrap_or_default(),
            // Compact, not the first line of the pretty form — that is just `{`, which makes for
            // a useless audit row.
            summary: serde_json::to_string(args)
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect(),
            kind: PreviewKind::Text,
        };
        async move { preview }
    }

    fn call(
        &self,
        ctx: &ToolCtx,
        args: Value,
    ) -> impl Future<Output = Result<ToolOutput, ToolError>> + Send;
}

/// Object-safe wrapper, since [`Tool`] uses RPITIT and cannot be made into a trait object.
///
/// Tools are written against `Tool`; `erase` adapts them. This keeps the boxing in one place
/// instead of forcing every implementation to be async-trait-shaped.
pub trait DynTool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters(&self) -> Value;
    fn permission(&self) -> Permission;
    fn needs_approval(&self, args: &Value) -> bool;
    fn preview<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: &'a Value,
    ) -> futures::future::BoxFuture<'a, ToolPreview>;
    fn call<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> futures::future::BoxFuture<'a, Result<ToolOutput, ToolError>>;
}

impl<T: Tool> DynTool for T {
    fn name(&self) -> &'static str {
        Tool::name(self)
    }
    fn description(&self) -> &'static str {
        Tool::description(self)
    }
    fn parameters(&self) -> Value {
        Tool::parameters(self)
    }
    fn permission(&self) -> Permission {
        Tool::permission(self)
    }
    fn needs_approval(&self, args: &Value) -> bool {
        Tool::needs_approval(self, args)
    }
    fn preview<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: &'a Value,
    ) -> futures::future::BoxFuture<'a, ToolPreview> {
        Box::pin(Tool::preview(self, ctx, args))
    }
    fn call<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> futures::future::BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(Tool::call(self, ctx, args))
    }
}

/// How a pending call was resolved by the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Approval {
    Allow,
    /// Allow this exact tool for the rest of the session without asking again.
    AllowAlways,
    Deny,
}

pub const MAX_DELEGATION_DEPTH: u8 = 5;

#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<&'static str, Arc<dyn DynTool>>,
}

impl ToolRegistry {
    /// The Phase 1 toolset. MCP-provided tools join the same map, namespaced `mcp:<server>:<tool>`,
    /// and pass through the same gate.
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        registry.register(builtins::ReadFile);
        registry.register(builtins::WriteFile);
        registry.register(builtins::ListDir);
        registry.register(builtins::SearchFiles);
        registry.register(builtins::RunCommand);
        registry
    }

    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        self.tools.insert(Tool::name(&tool), Arc::new(tool));
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn DynTool>> {
        self.tools.get(name).cloned()
    }

    /// The schemas an agent with `granted` may use. Tools it cannot call are never advertised —
    /// offering a tool and then refusing it wastes a turn and confuses the model.
    pub fn schemas_for(&self, granted: Permission) -> Vec<ToolSchema> {
        self.tools
            .values()
            .filter(|t| granted.contains(t.permission()))
            .map(|t| ToolSchema {
                name: t.name().to_owned(),
                description: t.description().to_owned(),
                parameters: t.parameters(),
            })
            .collect()
    }

    /// **The gate.** Every tool invocation in the application goes through here.
    ///
    /// `approved` is the decision already obtained from the user for calls that need one; the
    /// caller is responsible for asking, and this refuses to run anything unapproved.
    pub async fn dispatch(
        &self,
        name: &str,
        args: Value,
        ctx: &ToolCtx,
        granted: Permission,
        approved: Option<Approval>,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.get(name).ok_or_else(|| ToolError::Unknown(name.to_owned()))?;

        let needs = tool.permission();
        if !granted.contains(needs) {
            // Logged, not just returned: a run repeatedly reaching for a permission it was never
            // granted is the signal that an agent profile is misconfigured, and the model's own
            // narration of the failure is not something to trust for that.
            tracing::warn!(tool = name, ?needs, has = ?granted, "tool denied: missing permission");
            return Err(ToolError::Permission { tool: name.to_owned(), needs, has: granted });
        }

        if ctx.depth > MAX_DELEGATION_DEPTH {
            return Err(ToolError::TooDeep { limit: MAX_DELEGATION_DEPTH });
        }

        if tool.needs_approval(&args) {
            match approved {
                Some(Approval::Allow | Approval::AllowAlways) => {}
                Some(Approval::Deny) => {
                    tracing::info!(tool = name, "tool denied by the user");
                    return Err(ToolError::UserDenied);
                }
                // Not asked. Refuse rather than assume — this is what stops an approval-required
                // tool from running because a caller forgot the prompt.
                None => {
                    tracing::warn!(tool = name, "tool needs approval but was never asked about");
                    return Err(ToolError::UserDenied);
                }
            }
        }

        tracing::info!(tool = name, approved = ?approved, "tool running");
        tool.call(ctx, args).await
    }
}

/// Extract a required string argument, with an error the model can act on.
pub(crate) fn arg_str(args: &Value, key: &str, tool: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::BadArguments {
            tool: tool.to_owned(),
            reason: format!("missing required string field `{key}`"),
        })
}

/// A tool call the engine is holding until the user answers.
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub id: ToolCallId,
    pub tool: String,
    pub args: Value,
    pub preview: ToolPreview,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> ToolCtx {
        let tmp = std::env::temp_dir();
        ToolCtx {
            policy: PathPolicy::new(crate::fsaccess::AccessMode::FullSystem, [tmp.clone()], true),
            cwd: tmp,
            depth: 0,
        }
    }

    #[tokio::test]
    async fn permission_bits_block_a_tool_the_agent_lacks() {
        let registry = ToolRegistry::with_builtins();
        let err = registry
            .dispatch("run_command", json!({ "command": "echo hi" }), &ctx(), Permission::READ, None)
            .await
            .expect_err("READ-only agent must not run commands");

        assert!(matches!(err, ToolError::Permission { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn approval_required_tools_do_not_run_without_a_decision() {
        let registry = ToolRegistry::with_builtins();
        let path = std::env::temp_dir().join("meshflow-approval-test.txt");
        let _ = std::fs::remove_file(&path);

        let err = registry
            .dispatch(
                "write_file",
                json!({ "path": path.to_str().unwrap(), "content": "x" }),
                &ctx(),
                Permission::all(),
                None,
            )
            .await
            .expect_err("an unapproved write must not execute");

        assert!(matches!(err, ToolError::UserDenied));
        assert!(!path.exists(), "the file must not have been created");
    }

    #[tokio::test]
    async fn explicit_denial_prevents_execution() {
        let registry = ToolRegistry::with_builtins();
        let path = std::env::temp_dir().join("meshflow-denied-test.txt");
        let _ = std::fs::remove_file(&path);

        let err = registry
            .dispatch(
                "write_file",
                json!({ "path": path.to_str().unwrap(), "content": "x" }),
                &ctx(),
                Permission::all(),
                Some(Approval::Deny),
            )
            .await
            .expect_err("a denied write must not execute");

        assert!(matches!(err, ToolError::UserDenied));
        assert!(!path.exists());
    }

    #[test]
    fn agents_are_only_offered_tools_they_can_use() {
        let registry = ToolRegistry::with_builtins();

        let research: Vec<_> =
            registry.schemas_for(Permission::RESEARCH).into_iter().map(|s| s.name).collect();
        assert!(research.contains(&"read_file".to_owned()));
        assert!(!research.contains(&"write_file".to_owned()), "research agents cannot write");
        assert!(!research.contains(&"run_command".to_owned()));

        let coding: Vec<_> =
            registry.schemas_for(Permission::CODING).into_iter().map(|s| s.name).collect();
        assert!(coding.contains(&"run_command".to_owned()));
        assert!(coding.contains(&"write_file".to_owned()));
    }

    #[tokio::test]
    async fn unknown_tools_are_reported_not_ignored() {
        let registry = ToolRegistry::with_builtins();
        let err = registry
            .dispatch("nonexistent", json!({}), &ctx(), Permission::all(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Unknown(_)));
    }

    #[tokio::test]
    async fn delegation_depth_is_capped() {
        let registry = ToolRegistry::with_builtins();
        let deep = ToolCtx { depth: MAX_DELEGATION_DEPTH + 1, ..ctx() };
        let err = registry
            .dispatch("list_dir", json!({ "path": "." }), &deep, Permission::all(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::TooDeep { .. }));
    }
}
