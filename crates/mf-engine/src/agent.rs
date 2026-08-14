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
    context,
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
    /// Input tokens of history this model will accept. See [`crate::budget_for`].
    pub budget: usize,
    pub history: Arc<Mutex<Vec<Message>>>,
    pub pending: PendingApprovals,
    pub events: broadcast::Sender<EngineEvent>,
    /// Tools the user chose "always allow" for, for the life of this process.
    ///
    /// Shared with delegated sub-agents on purpose: the button says "always allow this *tool*",
    /// and re-asking per role would train the user to click through. A sub-agent is still bounded
    /// by the same path policy and by permissions that only ever narrow, so what it can do with
    /// the allowance is a subset of what the user already granted.
    pub always_allowed: Arc<Mutex<Vec<String>>>,
    /// While set, calls that would prompt run without one. Shared with every run in the process,
    /// so turning it off mid-flight stops the *next* call rather than only new conversations.
    pub auto_approve: Arc<std::sync::atomic::AtomicBool>,
    /// Suppresses the events that describe *the* run: `RunStarted`, `Delta`, `RunFinished`.
    ///
    /// Set for delegated sub-runs. The UI treats those three as the state of the one run the user
    /// started — a sub-run's `RunFinished` clears the approval queue and unsticks the composer
    /// while the parent is still working. Tool and approval events are *not* suppressed: the user
    /// still has to consent to what a sub-agent does, and the transcript still has to show it.
    pub quiet: bool,
    /// The role this run is playing, for events the user sees. `None` is the agent the user is
    /// addressing; `Some("research")` is a sub-agent it delegated to.
    pub agent: Option<String>,
}

impl<P: AiProvider> AgentRun<P> {
    /// Run the loop to completion, returning the model's last piece of text.
    ///
    /// The return value is what a delegated run answers with; the top-level run ignores it,
    /// having already streamed every word of it to the UI.
    pub async fn execute(self) -> String {
        if !self.quiet {
            let _ = self.events.send(EngineEvent::RunStarted { run: self.run, conv: self.conv });
        }

        let mut total = Usage::default();
        let mut stop = StopReason::EndTurn;
        let mut answer = String::new();

        for _ in 0..MAX_ITERATIONS {
            // Re-fitted every iteration rather than once per turn: tool results are what blow the
            // window, and they arrive *inside* the loop. The stored history is left whole — this
            // trims what is sent, not what the transcript keeps.
            let messages = {
                let history = self.history.lock().unwrap();
                let start = context::fit(&history, self.budget);
                if start > 0 {
                    // Mirrored to the in-app log viewer, so a user who notices the model has
                    // forgotten something earlier can see why.
                    tracing::info!(
                        dropped = start,
                        kept = history.len() - start,
                        budget = self.budget,
                        "history trimmed to fit the context window",
                    );
                }
                history[start..].to_vec()
            };

            let req = ChatRequest {
                model: self.model.clone(),
                system: Some(self.system.clone()),
                messages,
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
                // Kept as *the* answer, overwriting anything said before a tool call: the model's
                // last word is its conclusion, and a delegated run is judged on that alone.
                answer = turn.text.clone();
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

            let (results, fatal) = self.run_tools(turn.calls).await;
            // Committed before the check: an unanswered tool call is what makes the *next*
            // request invalid, and this history outlives the run.
            let message = Message { role: Role::Tool, content: results };
            self.persist(&message).await;
            self.history.lock().unwrap().push(message);

            if let Some(message) = fatal {
                tracing::warn!(%message, "run abandoned: the model cannot recover from this");
                let _ = self
                    .events
                    .send(EngineEvent::Error { run: Some(self.run), message });
                break;
            }
        }

        if !self.quiet {
            let _ = self.events.send(EngineEvent::RunFinished { run: self.run, stop, usage: total });
        }
        answer
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
            if !self.quiet {
                let _ = self.events.send(EngineEvent::Delta { run: self.run, event });
            }
        }

        Ok(turn)
    }

    /// Gate and execute every tool call, producing one result part per call.
    ///
    /// Every call gets a result, including failures — a provider rejects the next request if any
    /// tool call is left unanswered, so an error result is mandatory, not optional.
    ///
    /// The second return value is set when a failure is one the model cannot work around, such as
    /// delegation nested past its limit. Telling it and looping would just have it retry the same
    /// call until the iteration budget runs out.
    async fn run_tools(&self, calls: Vec<PendingCall>) -> (Vec<Part>, Option<String>) {
        let mut results = Vec::new();
        let mut fatal = None;

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
                agent: self.agent.clone(),
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
                            // The row has to answer "was anyone actually asked?" — see
                            // [`Approval::unattended`].
                            unattended: approval.is_some_and(Approval::unattended),
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
                Err(e) => {
                    if !e.is_recoverable() {
                        fatal = Some(e.to_string());
                    }
                    (e.to_string(), true, e.to_string())
                }
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

        (results, fatal)
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

        // Checked per call, not once per run: the user can turn the mode off while a run is in
        // flight, and the next destructive call must stop for them.
        if self.auto_approve.load(std::sync::atomic::Ordering::SeqCst) {
            // `warn`, not `info`. A write or a shell command running with nobody watching is the
            // line this app is otherwise built around, and the log is what makes it reviewable
            // afterwards.
            tracing::warn!(
                tool = name,
                agent = self.agent.as_deref().unwrap_or("main"),
                "auto-approved — auto-approve mode is on",
            );
            return Ok(Some(Approval::Auto));
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
            agent: self.agent.clone(),
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
        /// Messages in each request, in order — what the provider actually got sent.
        sent: Mutex<Vec<usize>>,
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
            req: ChatRequest,
        ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
            self.sent.lock().unwrap().push(req.messages.len());
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
        provider: Arc<ScriptedProvider>,
        _tmp: tempfile::TempDir,
    }

    fn harness(turns: Vec<Vec<StreamEvent>>) -> (AgentRun<ScriptedProvider>, Harness) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let (events, _) = broadcast::channel(256);
        let pending: PendingApprovals = Arc::default();
        let history = Arc::new(Mutex::new(vec![Message::user("go")]));
        let provider =
            Arc::new(ScriptedProvider { turns: Mutex::new(turns), sent: Mutex::default() });

        let run = AgentRun {
            run: RunId::new(),
            conv: ConvId::new(),
            store: None,
            provider: Arc::clone(&provider),
            model: "test".into(),
            system: "sys".into(),
            registry: Arc::new(ToolRegistry::with_builtins()),
            ctx: Arc::new(ToolCtx {
                policy: PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true),
                cwd: root,
                depth: 0,
                delegate: None,
            }),
            granted: Permission::all(),
            budget: 100_000,
            history: Arc::clone(&history),
            pending: Arc::clone(&pending),
            events: events.clone(),
            always_allowed: Arc::default(),
            auto_approve: Arc::default(),
            quiet: false,
            agent: None,
        };

        (run, Harness { events, pending, history, provider, _tmp: tmp })
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
            delegate: None,
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
    async fn auto_approve_runs_a_destructive_call_without_asking() {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("written.txt");
        let (mut run, h) = harness(vec![
            tool_turn(
                "c1",
                "write_file",
                &json!({ "path": path.to_str().unwrap(), "content": "hello" }).to_string(),
            ),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.ctx = Arc::new(ToolCtx {
            policy: PathPolicy::new(AccessMode::WorkspaceSandbox, [dir.clone()], true),
            cwd: dir,
            depth: 0,
            delegate: None,
        });
        run.auto_approve.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut rx = h.events.subscribe();

        // Completes with nobody answering anything — that is the whole feature.
        run.execute().await;

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        while let Ok(event) = rx.try_recv() {
            assert!(
                !matches!(event, EngineEvent::ApprovalNeeded { .. }),
                "auto-approve must not still be prompting",
            );
        }
    }

    #[tokio::test]
    async fn turning_auto_approve_off_stops_the_next_call_mid_run() {
        // The flag is read per call, not once per run, so a user who turns it off while an agent
        // is working is not ignored until the next conversation.
        let (run, h) = harness(vec![
            tool_turn("c1", "write_file", &json!({ "path": "n.txt", "content": "x" }).to_string()),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.auto_approve.store(true, std::sync::atomic::Ordering::SeqCst);
        let flag = Arc::clone(&run.auto_approve);
        flag.store(false, std::sync::atomic::Ordering::SeqCst);

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
    async fn a_history_over_budget_is_trimmed_before_it_reaches_the_provider() {
        let (mut run, h) = harness(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done(StopReason::EndTurn),
        ]]);
        run.budget = 1_000;
        // Ten user turns of ~1000 tokens each, so only the newest can fit.
        *h.history.lock().unwrap() =
            (0..10).map(|i| Message::user(format!("{i}{}", "x".repeat(4_000)))).collect();

        run.execute().await;

        let sent = h.provider.sent.lock().unwrap().clone();
        assert_eq!(sent, vec![1], "the provider must see the trimmed slice, not the whole history");
        // Trimming shapes the request only — the transcript keeps every turn.
        assert_eq!(h.history.lock().unwrap().len(), 11, "10 user turns + the reply");
    }

    #[tokio::test]
    async fn an_unrecoverable_tool_error_stops_the_run_instead_of_looping() {
        // Past the delegation limit every tool refuses, and no amount of retrying changes that —
        // without this the model spends all 25 iterations being told the same thing.
        let (mut run, h) = harness(vec![
            tool_turn("c1", "list_dir", &json!({ "path": "." }).to_string()),
            tool_turn("c2", "list_dir", &json!({ "path": "." }).to_string()),
        ]);
        run.ctx = Arc::new(ToolCtx {
            policy: PathPolicy::new(AccessMode::WorkspaceSandbox, [run.ctx.cwd.clone()], true),
            cwd: run.ctx.cwd.clone(),
            depth: crate::tool::MAX_DELEGATION_DEPTH + 1,
            delegate: None,
        });
        let mut rx = h.events.subscribe();

        run.execute().await;

        // The tool result is still recorded — an unanswered call would make the stored
        // conversation unusable — but the loop stops after the first round.
        let calls = h.provider.sent.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "the run kept going after an unrecoverable error");

        let mut reported = false;
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::Error { message, .. } = event {
                assert!(message.contains("nested too deeply"), "{message}");
                reported = true;
            }
        }
        assert!(reported, "the user is told why the run stopped");
    }

    #[tokio::test]
    async fn a_sub_agents_prompts_name_the_agent_that_is_asking() {
        let (mut run, h) = harness(vec![
            tool_turn("c1", "write_file", &json!({ "path": "n.txt", "content": "x" }).to_string()),
            vec![StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.agent = Some("research".into());
        let mut rx = h.events.subscribe();
        let pending = Arc::clone(&h.pending);
        let task = tokio::spawn(run.execute());

        let mut labelled = 0;
        loop {
            match rx.recv().await.unwrap() {
                EngineEvent::ApprovalNeeded { call, agent, .. } => {
                    // Consenting to a write from an agent the user never addressed is a
                    // different decision, so the modal has to be able to say so.
                    assert_eq!(agent.as_deref(), Some("research"));
                    labelled += 1;
                    pending.lock().unwrap().remove(&call).unwrap().send(Approval::Allow).unwrap();
                }
                EngineEvent::ToolStarted { agent, .. } => {
                    assert_eq!(agent.as_deref(), Some("research"));
                    labelled += 1;
                    break;
                }
                _ => {}
            }
        }
        task.await.unwrap();
        assert_eq!(labelled, 2);
    }

    #[tokio::test]
    async fn a_quiet_run_reports_its_tools_but_not_its_lifecycle() {
        let (mut run, h) = harness(vec![
            tool_turn("c1", "list_dir", &json!({ "path": "." }).to_string()),
            vec![StreamEvent::TextDelta("found it".into()), StreamEvent::Done(StopReason::EndTurn)],
        ]);
        run.quiet = true;
        let mut rx = h.events.subscribe();

        let answer = run.execute().await;
        assert_eq!(answer, "found it", "a delegated run is judged on its last message");

        let mut tool_events = 0;
        while let Ok(event) = rx.try_recv() {
            match event {
                // These three are how the UI tracks *the* run the user started. A sub-run
                // emitting RunFinished would clear the parent's pending approvals.
                EngineEvent::RunStarted { .. }
                | EngineEvent::RunFinished { .. }
                | EngineEvent::Delta { .. } => panic!("a quiet run leaked {event:?}"),
                EngineEvent::ToolStarted { .. } | EngineEvent::ToolFinished { .. } => {
                    tool_events += 1;
                }
                _ => {}
            }
        }
        assert_eq!(tool_events, 2, "what a sub-agent does must still reach the transcript");
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
