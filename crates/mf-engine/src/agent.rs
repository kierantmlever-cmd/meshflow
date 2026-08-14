//! The agentic turn loop.
//!
//! One user message drives a loop: stream the model → collect tool calls → gate each on
//! permissions and (where required) user approval → execute → feed results back → repeat, until
//! the model stops asking for tools or a budget is exhausted.
//!
//! Approval is a *blocking* step by design. The run parks on a oneshot until the user answers;
//! there is no timeout that proceeds on its own, because a timeout that auto-approves is not
//! consent and a timeout that auto-denies silently breaks long-running work.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::{
    proto::{ConvId, EngineEvent, RunId, StopReason, StreamEvent, ToolCallId, Usage},
    provider::{AiProvider, ChatRequest, Message, Part, ProviderError, Role},
    store::{AuditEntry, Store},
    tool::{Approval, Permission, ToolCtx, ToolError, ToolRegistry},
};

/// Hard ceiling on model round-trips in one turn. Without it, a model that keeps calling tools
/// bills forever.
const MAX_ITERATIONS: usize = 25;

/// Approvals the run is waiting on, keyed by call id. Shared with the engine loop, which
/// resolves them when the user answers.
pub type PendingApprovals = Arc<Mutex<HashMap<ToolCallId, oneshot::Sender<Approval>>>>;

pub struct AgentRun<P: AiProvider> {
    pub run: RunId,
    pub conv: ConvId,
    /// `None` when persistence failed to initialise — the run still works, it just isn't saved.
    pub store: Option<Store>,
    pub provider: Arc<P>,
    pub model: String,
    pub system: String,
    pub registry: Arc<ToolRegistry>,
    pub ctx: Arc<ToolCtx>,
    pub granted: Permission,
    pub history: Arc<Mutex<Vec<Message>>>,
    pub pending: PendingApprovals,
    pub events: broadcast::Sender<EngineEvent>,
    /// Tools the user chose "always allow" for, for the life of this process.
    pub always_allowed: Arc<Mutex<Vec<String>>>,
}

impl<P: AiProvider> AgentRun<P> {
    pub async fn execute(self) {
        let _ = self.events.send(EngineEvent::RunStarted { run: self.run, conv: self.conv });

        let mut total = Usage::default();
        let mut stop = StopReason::EndTurn;

        for _ in 0..MAX_ITERATIONS {
            let req = ChatRequest {
                model: self.model.clone(),
                system: Some(self.system.clone()),
                messages: self.history.lock().unwrap().clone(),
                tools: self.registry.schemas_for(self.granted),
                ..Default::default()
            };

            let turn = match self.stream_turn(req).await {
                Ok(turn) => turn,
                Err(e) => {
                    let _ = self
                        .events
                        .send(EngineEvent::Error { run: Some(self.run), message: e.to_string() });
                    break;
                }
            };

            total.input_tokens += turn.usage.input_tokens;
            total.output_tokens += turn.usage.output_tokens;
            stop = turn.stop;

            // Commit the assistant turn before running anything: if a tool panics or the app
            // dies mid-execution, the transcript still shows what the model asked for.
            let mut assistant = Vec::new();
            if !turn.text.is_empty() {
                assistant.push(Part::Text(turn.text));
            }
            for call in &turn.calls {
                assistant.push(Part::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    args: call.parsed_args(),
                });
            }
            if !assistant.is_empty() {
                let message = Message { role: Role::Assistant, content: assistant };
                self.persist(&message).await;
                self.history.lock().unwrap().push(message);
            }

            if turn.calls.is_empty() {
                break;
            }

            let results = self.run_tools(turn.calls).await;
            let message = Message { role: Role::Tool, content: results };
            self.persist(&message).await;
            self.history.lock().unwrap().push(message);
        }

        let _ = self.events.send(EngineEvent::RunFinished { run: self.run, stop, usage: total });
    }

    /// Write a turn to the store, if there is one. A persistence failure is logged and the run
    /// continues: losing history is bad, dropping the user's in-flight work is worse.
    async fn persist(&self, message: &Message) {
        if let Some(store) = &self.store
            && let Err(e) = store.append_message(self.conv, message).await
        {
            tracing::error!(%e, "could not persist message");
        }
    }

    /// Consume one model response, forwarding deltas as they arrive.
    async fn stream_turn(&self, req: ChatRequest) -> Result<Turn, ProviderError> {
        let mut stream = self.provider.stream(req).await?;
        let mut turn = Turn::default();

        while let Some(event) = stream.next().await {
            let event = event?;
            match &event {
                StreamEvent::TextDelta(t) => turn.text.push_str(t),
                StreamEvent::ToolCallStart { id, name } => {
                    turn.calls.push(PendingCall {
                        id: id.clone(),
                        name: name.clone(),
                        args_json: String::new(),
                    });
                }
                StreamEvent::ToolCallDelta { id, args_json } => {
                    // Fragments can interleave across parallel calls, so match on id rather
                    // than assuming the last-opened call.
                    if let Some(call) = turn.calls.iter_mut().find(|c| &c.id == id) {
                        call.args_json.push_str(args_json);
                    }
                }
                StreamEvent::Usage(u) => turn.usage = *u,
                StreamEvent::Done(reason) => turn.stop = *reason,
                _ => {}
            }
            let _ = self.events.send(EngineEvent::Delta { run: self.run, event });
        }

        Ok(turn)
    }

    /// Gate and execute every tool call, producing one result part per call.
    ///
    /// Every call gets a result, including failures — a provider rejects the next request if any
    /// tool call is left unanswered, so an error result is mandatory, not optional.
    async fn run_tools(&self, calls: Vec<PendingCall>) -> Vec<Part> {
        let mut results = Vec::new();

        for call in calls {
            let id = ToolCallId::new();
            let args = call.parsed_args();

            let approval = match self.approval_for(&call.name, &args, id).await {
                Ok(approval) => approval,
                Err(part) => {
                    results.push(part);
                    continue;
                }
            };

            let _ = self.events.send(EngineEvent::ToolStarted {
                run: self.run,
                call: id,
                tool: call.name.clone(),
            });

            // Written *before* execution, so a tool that hangs or takes the process down is
            // still attributable afterwards. `ok` is filled in when it returns.
            let audit_id = match &self.store {
                Some(store) => {
                    // The one-line summary, not `detail`: for a write, detail is the whole diff,
                    // and storing that on every call bloats the database without making the
                    // record more useful. "overwrite /path (+12 −3)" is the auditable fact.
                    let detail = match self.registry.get(&call.name) {
                        Some(tool) => tool.preview(&self.ctx, &args).await.summary,
                        None => String::new(),
                    };
                    store
                        .audit(AuditEntry {
                            action: "tool",
                            tool: Some(&call.name),
                            detail: Some(&detail),
                            approved: approval.map(|a| a != Approval::Deny),
                            elevated: false,
                            ok: None,
                        })
                        .await
                        .inspect_err(|e| tracing::error!(%e, "could not write audit entry"))
                        .ok()
                }
                None => None,
            };

            let outcome = self
                .registry
                .dispatch(&call.name, args, &self.ctx, self.granted, approval)
                .await;

            let (content, is_error, summary) = match outcome {
                Ok(out) => (out.content, false, out.summary),
                // The error text goes back to the model: a denied path or a bad argument is
                // information it can act on, not a reason to abandon the run.
                Err(e) => (e.to_string(), true, e.to_string()),
            };

            if let (Some(store), Some(audit_id)) = (&self.store, audit_id)
                && let Err(e) = store.audit_complete(audit_id, !is_error).await
            {
                tracing::error!(%e, "could not complete audit entry");
            }

            let _ = self.events.send(EngineEvent::ToolFinished {
                run: self.run,
                call: id,
                ok: !is_error,
                summary,
            });

            results.push(Part::ToolResult { id: call.id, content, is_error });
        }

        results
    }

    /// Ask the user if this call needs it. Returns the decision, or a ready-made error result
    /// when the tool is unknown or the UI hung up.
    async fn approval_for(
        &self,
        name: &str,
        args: &Value,
        id: ToolCallId,
    ) -> Result<Option<Approval>, Part> {
        let Some(tool) = self.registry.get(name) else {
            // Let dispatch produce the canonical "no such tool" error.
            return Ok(None);
        };

        // Permission first, approval second. Asking the user to approve a call the agent has no
        // permission to make is consent theatre: the answer cannot change the outcome, and
        // routinely prompting for calls that will be refused anyway trains users to click
        // through. Dispatch stays the authority and emits the canonical error.
        if !self.granted.contains(tool.permission()) {
            return Ok(None);
        }

        if !tool.needs_approval(args) {
            return Ok(None);
        }
        if self.always_allowed.lock().unwrap().iter().any(|t| t == name) {
            // Logged: a destructive tool running without a prompt must be explicable afterwards,
            // and "I don't remember choosing that" is exactly the complaint this answers.
            tracing::info!(tool = name, "auto-approved — user chose 'always allow' this session");
            return Ok(Some(Approval::Allow));
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        tracing::info!(tool = name, "waiting for user approval");
        let _ = self.events.send(EngineEvent::ApprovalNeeded {
            run: self.run,
            call: id,
            tool: name.to_owned(),
            preview: tool.preview(&self.ctx, args).await,
        });

        match rx.await {
            Ok(Approval::AllowAlways) => {
                self.always_allowed.lock().unwrap().push(name.to_owned());
                Ok(Some(Approval::AllowAlways))
            }
            Ok(decision) => Ok(Some(decision)),
            // Sender dropped: the app is shutting down or the run was cancelled. Treat as a
            // denial — never as consent.
            Err(_) => Err(Part::ToolResult {
                id: id.to_string(),
                content: ToolError::UserDenied.to_string(),
                is_error: true,
            }),
        }
    }
}

#[derive(Default)]
struct Turn {
    text: String,
    calls: Vec<PendingCall>,
    usage: Usage,
    stop: StopReason,
}

struct PendingCall {
    id: String,
    name: String,
    args_json: String,
}

impl PendingCall {
    /// Providers stream arguments as JSON fragments; a truncated stream leaves invalid JSON.
    /// An empty object lets the tool report a specific "missing field" error the model can fix,
    /// which beats failing the whole run on a parse error.
    fn parsed_args(&self) -> Value {
        serde_json::from_str(&self.args_json).unwrap_or_else(|_| Value::Object(Default::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fsaccess::{AccessMode, PathPolicy},
        provider::{Capabilities, ModelInfo, ProviderKind},
    };
    use futures::stream::BoxStream;
    use serde_json::json;

    /// Replays canned turns so the loop can be tested without a network or a model.
    struct ScriptedProvider {
        turns: Mutex<Vec<Vec<StreamEvent>>>,
    }

    impl AiProvider for ScriptedProvider {
        fn kind(&self) -> ProviderKind {
            ProviderKind::OpenAi
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }
        async fn models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
        async fn stream(
            &self,
            _req: ChatRequest,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            let turn = {
                let mut turns = self.turns.lock().unwrap();
                if turns.is_empty() { Vec::new() } else { turns.remove(0) }
            };
            Ok(futures::stream::iter(turn.into_iter().map(Ok)).boxed())
        }
    }

    struct Harness {
        events: broadcast::Sender<EngineEvent>,
        pending: PendingApprovals,
        history: Arc<Mutex<Vec<Message>>>,
        _tmp: tempfile::TempDir,
    }

    fn harness(turns: Vec<Vec<StreamEvent>>) -> (AgentRun<ScriptedProvider>, Harness) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (events, _) = broadcast::channel(256);
        let pending: PendingApprovals = Arc::default();
        let history = Arc::new(Mutex::new(vec![Message::user("go")]));

        let run = AgentRun {
            run: RunId::new(),
            conv: ConvId::new(),
            store: None,
            provider: Arc::new(ScriptedProvider { turns: Mutex::new(turns) }),
            model: "test".into(),
            system: "sys".into(),
            registry: Arc::new(ToolRegistry::with_builtins()),
            ctx: Arc::new(ToolCtx {
                policy: PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true),
                cwd: root,
                depth: 0,
            }),
            granted: Permission::all(),
            history: Arc::clone(&history),
            pending: Arc::clone(&pending),
            events: events.clone(),
            always_allowed: Arc::default(),
        };

        (run, Harness { events, pending, history, _tmp: tmp })
    }

    fn tool_turn(id: &str, name: &str, args: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::ToolCallStart { id: id.into(), name: name.into() },
            StreamEvent::ToolCallDelta { id: id.into(), args_json: args.into() },
            StreamEvent::ToolCallEnd { id: id.into() },
            StreamEvent::Done(StopReason::ToolUse),
        ]
    }

    #[tokio::test]
    async fn plain_text_turn_finishes_without_tools() {
        let (run, h) = harness(vec![vec![
            StreamEvent::TextDelta("hello".into()),
            StreamEvent::Done(StopReason::EndTurn),
        ]]);
        let mut rx = h.events.subscribe();
        run.execute().await;

        let mut finished = false;
        while let Ok(ev) = rx.try_recv() {
            if let EngineEvent::RunFinished { stop, .. } = ev {
                assert_eq!(stop, StopReason::EndTurn);
                finished = true;
            }
        }
        assert!(finished);

        let history = h.history.lock().unwrap();
        assert_eq!(history.len(), 2, "user + assistant");
    }

    #[tokio::test]
    async fn executes_an_approval_free_tool_and_feeds_the_result_back() {
        let path = {
            let tmp = tempfile::tempdir().unwrap();
            tmp.keep().join("x")
        };
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("a.txt"), "contents").unwrap();

        let (mut run, h) = harness(vec![
            tool_turn("c1", "read_file", &json!({ "path": "a.txt" }).to_string()),
            vec![StreamEvent::TextDelta("done".into()), StreamEvent::Done(StopReason::EndTurn)],
        ]);
        // Point the sandbox at the directory holding the file.
        run.ctx = Arc::new(ToolCtx {
            policy: PathPolicy::new(AccessMode::WorkspaceSandbox, [path.clone()], true),
            cwd: path,
            depth: 0,
        });

        run.execute().await;

        let history = h.history.lock().unwrap();
        let result = history
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|p| match p {
                Part::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("a tool result was appended");
        assert_eq!(result.0, "contents");
        assert!(!result.1);
    }

    #[tokio::test]
    async fn approval_required_tool_blocks_until_the_user_answers() {
        let (run, h) = harness(vec![
            tool_turn("c1", "write_file", &json!({ "path": "new.txt", "content": "x" }).to_string()),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        let mut rx = h.events.subscribe();
        let pending = Arc::clone(&h.pending);

        let task = tokio::spawn(run.execute());

        // Wait for the ask, then answer it — this is what unblocks the run.
        let call = loop {
            match rx.recv().await.unwrap() {
                EngineEvent::ApprovalNeeded { call, tool, preview, .. } => {
                    assert_eq!(tool, "write_file");
                    assert!(preview.detail.contains('x'), "preview shows the content");
                    break call;
                }
                EngineEvent::RunFinished { .. } => panic!("run finished without asking"),
                _ => {}
            }
        };

        pending.lock().unwrap().remove(&call).unwrap().send(Approval::Allow).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn denial_is_reported_to_the_model_rather_than_ending_the_run() {
        let (run, h) = harness(vec![
            tool_turn("c1", "write_file", &json!({ "path": "n.txt", "content": "x" }).to_string()),
            vec![StreamEvent::TextDelta("ok".into()), StreamEvent::Done(StopReason::EndTurn)],
        ]);
        let mut rx = h.events.subscribe();
        let pending = Arc::clone(&h.pending);
        let task = tokio::spawn(run.execute());

        loop {
            if let EngineEvent::ApprovalNeeded { call, .. } = rx.recv().await.unwrap() {
                pending.lock().unwrap().remove(&call).unwrap().send(Approval::Deny).unwrap();
                break;
            }
        }
        task.await.unwrap();

        let history = h.history.lock().unwrap();
        let (content, is_error) = history
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|p| match p {
                Part::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .expect("denial produces a tool result");
        assert!(is_error);
        assert!(content.contains("denied"), "{content}");
    }

    #[tokio::test]
    async fn dropping_the_approval_channel_denies_rather_than_allows() {
        let (run, h) = harness(vec![
            tool_turn("c1", "write_file", &json!({ "path": "n.txt", "content": "x" }).to_string()),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        let mut rx = h.events.subscribe();
        let pending = Arc::clone(&h.pending);
        let task = tokio::spawn(run.execute());

        loop {
            if let EngineEvent::ApprovalNeeded { call, .. } = rx.recv().await.unwrap() {
                // Shutdown mid-ask: drop the sender without answering.
                drop(pending.lock().unwrap().remove(&call).unwrap());
                break;
            }
        }
        task.await.unwrap();

        let history = h.history.lock().unwrap();
        let denied = history.iter().flat_map(|m| &m.content).any(|p| {
            matches!(p, Part::ToolResult { is_error: true, .. })
        });
        assert!(denied, "a dropped approval channel must never be read as consent");
    }

    #[tokio::test]
    async fn permission_errors_come_back_as_tool_results_not_run_failures() {
        let (mut run, h) = harness(vec![
            tool_turn("c1", "run_command", &json!({ "command": "echo hi" }).to_string()),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.granted = Permission::READ; // no EXEC
        let mut rx = h.events.subscribe();

        // Completes without anyone answering an approval: a call the agent has no permission
        // to make must never park the run on a prompt.
        run.execute().await;

        while let Ok(ev) = rx.try_recv() {
            assert!(
                !matches!(ev, EngineEvent::ApprovalNeeded { .. }),
                "must not ask the user to approve a call that permissions already forbid"
            );
        }

        let history = h.history.lock().unwrap();
        let content = history
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|p| match p {
                Part::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("permission failure still produces a result");
        assert!(content.contains("permission denied"), "{content}");
    }

    #[tokio::test]
    async fn truncated_tool_arguments_do_not_abort_the_run() {
        // A stream cut mid-fragment leaves invalid JSON; the tool should report a missing
        // field rather than the run dying on a parse error.
        let (run, h) = harness(vec![
            tool_turn("c1", "read_file", "{\"pa"),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.execute().await;

        let history = h.history.lock().unwrap();
        let content = history
            .iter()
            .flat_map(|m| &m.content)
            .find_map(|p| match p {
                Part::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("malformed arguments still produce a result");
        assert!(content.contains("missing required string field"), "{content}");
    }

    #[tokio::test]
    async fn parallel_tool_calls_each_get_a_result() {
        let (run, h) = harness(vec![
            vec![
                StreamEvent::ToolCallStart { id: "a".into(), name: "list_dir".into() },
                StreamEvent::ToolCallDelta { id: "a".into(), args_json: "{\"path\":\".\"}".into() },
                StreamEvent::ToolCallStart { id: "b".into(), name: "list_dir".into() },
                StreamEvent::ToolCallDelta { id: "b".into(), args_json: "{\"path\":\".\"}".into() },
                StreamEvent::Done(StopReason::ToolUse),
            ],
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.execute().await;

        let history = h.history.lock().unwrap();
        let ids: Vec<_> = history
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|p| match p {
                Part::ToolResult { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        // Providers reject the next request if any call is left unanswered.
        assert_eq!(ids, vec!["a", "b"]);
    }
}
