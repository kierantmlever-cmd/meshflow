//! MeshFlow engine.
//!
//! Runs on a multi-threaded Tokio runtime. Owns providers, agents, tools and storage.
//! Knows nothing about the UI beyond [`proto`] — deliberately, because Freya's reactivity is
//! single-threaded and `!Send`, so the two worlds can never share state directly.

pub mod agent;
pub mod config;
pub mod context;
pub mod diff;
pub mod files;
pub mod fsaccess;
pub mod logging;
pub mod paths;
pub mod proto;
pub mod provider;
pub mod search;
pub mod secrets;
pub mod store;
pub mod tool;

/// Re-exported so the settings UI can build a [`secrecy::SecretString`] without depending on the
/// crate directly — and so it stays the *same* version, since a mismatch would silently downgrade
/// the redacting `Debug` that keeps keys out of logs.
pub use secrecy;

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use secrecy::SecretString;
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

use agent::{AgentRun, PendingApprovals};
use config::{Config, ProviderEntry};
use fsaccess::{AccessMode, PathPolicy};
use proto::{
    ConvId, EngineCommand, EngineEvent, ProviderId, ProviderSummary, RunId, StopReason, Usage,
};
use provider::{AiProvider, AnyProvider, ProviderConfig, ProviderKind};
use store::Store;
use tool::{Delegator, Permission, ToolCtx, ToolRegistry};

/// What the UI is told when a message is sent with nothing configured. Points at the fix rather
/// than just naming the problem — this is the first thing a new user sees.
const NO_PROVIDER: &str =
    "No AI provider configured. Open Settings and add one, or set MESHFLOW_API_KEY.";

/// A resolved provider, the model to talk to, and the history budget that model allows.
type Active = (Arc<AnyProvider>, String, usize);

/// Assumed window when nothing reported one. Small enough for a local 8B model to survive and
/// for anything larger to merely under-use its window — the failure this avoids is one-sided,
/// since guessing high means a rejected turn while guessing low only forgets sooner.
const DEFAULT_CONTEXT_WINDOW: u32 = 32_000;

/// The share of a model's window that history may occupy.
///
/// The rest covers the system prompt, the tool schemas and the reply, all of which count against
/// the same window.
///
/// ponytail: a flat 75/25 split, not arithmetic on the real reserve — the reply ceiling is a
/// provider-side default (`max_tokens`) rather than something this layer knows. Subtract the
/// actual number if a small-window model ever has its reply truncated.
pub fn budget_for(context_window: Option<u32>) -> usize {
    context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW) as usize / 4 * 3
}

/// Layer 1 of the instruction hierarchy. The live path policy is appended at run time so the
/// model learns its limits from the prompt rather than from a failed tool call.
const SYSTEM_PROMPT: &str = "You are MeshFlow, a local AI coding assistant. Be concise. \
     Use markdown, and fenced code blocks with a language tag for code. \
     Use the provided tools to inspect and modify files rather than guessing at their contents.";

/// Appended to a sub-agent's system prompt.
///
/// It has no conversation to fall back on and no way to ask, so it is told both — a sub-agent
/// that ends its turn with "which file did you mean?" has burned the delegation.
const SUB_AGENT_PROMPT: &str = "You are a sub-agent working on one delegated task. You cannot ask \
     questions: the task text is the whole of your context. Only your final message is returned \
     to the agent that delegated to you, so make it a complete answer rather than a summary of \
     what you did.";

/// What the top-level agent may do. `AGENT` is what puts `delegate` in its toolbox.
const TOP_LEVEL: Permission = Permission::CODING.union(Permission::AGENT);

/// Hard ceiling on the per-task agent count, whatever the UI asks for.
///
/// Every sub-agent is a full model run that can spawn more, so this is a spend limit as much as a
/// concurrency one. Clamped here rather than trusted from the command: the engine is what bills.
pub const MAX_AGENTS: u8 = 8;

/// The sub-agents one task may still use, shared by every agent in its tree.
///
/// One pool for the whole task, not a fresh allowance per level — otherwise "at most 3" means 3
/// at the top, 9 below it, 27 below that, which is not what anyone setting the number meant.
#[derive(Clone)]
struct AgentPool(Arc<std::sync::atomic::AtomicU8>);

impl AgentPool {
    fn new(size: u8) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicU8::new(size.min(MAX_AGENTS))))
    }

    /// How many are still available. Used to decide whether a sub-agent is even offered
    /// `delegate` — a tool that can only fail wastes a turn and reads as a broken environment.
    fn remaining(&self) -> u8 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Claim one, if any are left. Atomic, because sibling agents can delegate at the same time
    /// and a check-then-decrement would let both through on the last one.
    fn take(&self) -> bool {
        self.0
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |left| left.checked_sub(1),
            )
            .is_ok()
    }
}

/// Tells the agent its allowance in the words the refusal will use, so it plans within the number
/// instead of discovering it by being turned down.
fn agent_budget_prompt(max_agents: u8) -> String {
    match max_agents {
        0 => "You have no sub-agents for this task. Do the work yourself.".to_owned(),
        1 => "You may hand at most 1 task to a sub-agent. Delegate only if the work genuinely \
              splits; otherwise do it yourself."
            .to_owned(),
        n => format!(
            "You may use at most {n} sub-agents for this task, counting any they spawn in turn. \
             That is a ceiling, not a target: use only as many as the work actually needs, and do \
             the rest yourself."
        ),
    }
}

/// Runs sub-agents for the `delegate` tool.
///
/// Holds everything a run needs, so that delegating is the same code path as a top-level turn
/// rather than a second, quieter implementation of it that drifts.
#[derive(Clone)]
struct SubAgents {
    provider: Arc<AnyProvider>,
    model: String,
    system: String,
    registry: Arc<ToolRegistry>,
    policy: fsaccess::PathPolicy,
    cwd: PathBuf,
    store: Option<Store>,
    events: broadcast::Sender<EngineEvent>,
    pending: PendingApprovals,
    always_allowed: Arc<Mutex<Vec<String>>>,
    /// Shared, not copied: switching the mode off has to reach sub-agents already running.
    auto_approve: Arc<std::sync::atomic::AtomicBool>,
    /// What the *parent* holds. The ceiling on what any sub-agent it spawns can be granted.
    granted: Permission,
    budget: usize,
    /// Sub-agents left for this task. Shared with every child, so the whole tree draws on it.
    pool: AgentPool,
    /// What the user set, for the refusal message — the pool itself only knows what is left.
    max_agents: u8,
}

impl Delegator for SubAgents {
    fn run<'a>(
        &'a self,
        task: String,
        role: &'static str,
        permission: Permission,
        depth: u8,
    ) -> futures::future::BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            // Claimed before anything is set up, so two siblings racing for the last slot cannot
            // both get one. Recoverable on purpose: the agent is expected to absorb the work.
            if !self.pool.take() {
                tracing::info!(role, "delegation refused: the task's agent budget is spent");
                return Err(format!(
                    "All {} sub-agents for this task have been used. Do the remaining work \
                     yourself.",
                    self.max_agents,
                ));
            }

            // `AGENT` rides along with the role, so a sub-agent can split its own work up in
            // turn — none of the role presets carry it, and without this the depth cap, the
            // nested delegator below and `TooDeep` would all be unreachable code. What bounds
            // the tree is the depth cap, not the permission bits; every other permission is
            // still narrowed to the role.
            //
            // Withheld once the pool is empty, though: with a cap of 1 the sub-agent would
            // otherwise be handed a `delegate` whose every call is refused, and it spends a turn
            // finding that out.
            let inheritable = match self.pool.remaining() {
                0 => permission,
                _ => permission | Permission::AGENT,
            };
            let granted = self.granted.delegated(inheritable);
            tracing::info!(role, ?granted, depth, "delegating");

            // Its own conversation, not the parent's. Interleaving a sub-agent's turns into the
            // transcript would leave a message sequence no provider would accept on reload —
            // and a user reading the thread would see two agents talking over each other.
            let conv = ConvId::new();
            if let Some(store) = &self.store {
                let title = format!(
                    "delegated: {}",
                    task.lines().next().unwrap_or_default().chars().take(80).collect::<String>(),
                );
                if let Err(e) = store.create_conversation(conv, &title).await {
                    tracing::error!(%e, "could not record the delegated conversation");
                }
            }

            // The sub-agent can delegate in turn, but only ever downwards: its own delegator is
            // capped at what it was itself granted.
            let child = Arc::new(Self { granted, ..self.clone() });
            let ctx = Arc::new(ToolCtx {
                policy: self.policy.clone(),
                cwd: self.cwd.clone(),
                depth,
                delegate: Some(child),
            });

            let run = AgentRun {
                run: RunId::new(),
                conv,
                store: self.store.clone(),
                provider: Arc::clone(&self.provider),
                model: self.model.clone(),
                // The allowance is recomputed from what is *left*, not inherited: a sub-agent
                // told "you may use at most 3" out of a pool its siblings have already drained
                // plans around agents it cannot have.
                system: format!(
                    "{}\n\n{}\n\n{SUB_AGENT_PROMPT}",
                    self.system,
                    agent_budget_prompt(self.pool.remaining()),
                ),
                registry: Arc::clone(&self.registry),
                ctx,
                granted,
                budget: self.budget,
                history: Arc::new(Mutex::new(vec![provider::Message::user(task)])),
                pending: Arc::clone(&self.pending),
                events: self.events.clone(),
                always_allowed: Arc::clone(&self.always_allowed),
                auto_approve: Arc::clone(&self.auto_approve),
                quiet: true,
                agent: Some(role.to_owned()),
            };

            match run.execute().await.trim() {
                // Reported as a failure rather than returned as an empty string: the parent would
                // read `""` as an answer and carry on as if the work were done.
                "" => Err("the sub-agent finished without producing an answer".into()),
                answer => Ok(answer.to_owned()),
            }
        })
    }
}

/// Everything derived from the active workspace root.
///
/// Rebuilt in full on every switch rather than patched, because the system prompt restates the
/// live path policy: a stale copy would keep telling the model it can reach a directory the
/// policy has stopped allowing, and the model would learn otherwise from a failed tool call.
struct Sandbox {
    ctx: Arc<ToolCtx>,
    system: String,
    /// The effective root — the workspace when one is set, the working directory otherwise.
    root: PathBuf,
}

impl Sandbox {
    /// Build the sandbox for `root`, falling back to the process working directory.
    ///
    /// A root that no longer resolves returns a *warning*, not an error. `PathPolicy` drops roots
    /// it cannot canonicalise, so accepting one would leave an empty root list that denies every
    /// path — and the user would see "outside the allowed roots" for a directory that is simply
    /// not there any more.
    fn open(root: Option<PathBuf>) -> (Self, Option<String>) {
        let (policy, warn) = match root {
            Some(root) => match root.canonicalize() {
                Ok(resolved) if resolved.is_dir() => {
                    (PathPolicy::new(AccessMode::WorkspaceSandbox, [resolved], true), None)
                }
                _ => (
                    PathPolicy::cwd(true),
                    Some(format!(
                        "Workspace {} is not reachable — it may have been moved, deleted or \
                         unmounted. Falling back to the working directory.",
                        root.display()
                    )),
                ),
            },
            None => (PathPolicy::cwd(true), None),
        };

        let root = policy.roots().first().cloned().unwrap_or_else(|| ".".into());
        let system = format!("{SYSTEM_PROMPT}\n\n{}", policy.describe());
        // No delegator: this is the sandbox the editor and the search panel share. A run builds
        // its own context on top of these, because delegation needs the provider and the model,
        // which belong to the run rather than to the workspace.
        let ctx = Arc::new(ToolCtx { cwd: root.clone(), policy, depth: 0, delegate: None });
        (Self { ctx, system, root }, warn)
    }
}

/// The engine loop. Owns routing only — every turn is a spawned task on the Tokio pool, so a
/// slow provider or a tool awaiting approval can never stall command dispatch.
pub async fn run(
    mut cmd_rx: mpsc::UnboundedReceiver<EngineCommand>,
    evt_tx: broadcast::Sender<EngineEvent>,
) {
    // Read directly rather than through `resolve_provider`'s load: a config too malformed to parse
    // must still leave a usable sandbox, and the working directory is the safe answer.
    let saved_root = Config::load().ok().and_then(|c| c.active_workspace);
    let (mut sandbox, warn) = Sandbox::open(saved_root);
    tracing::info!(
        root = %sandbox.root.display(),
        mode = ?sandbox.ctx.policy.mode(),
        "engine started",
    );
    if let Some(warn) = warn {
        tracing::warn!(%warn, "falling back to the working directory");
        let _ = evt_tx.send(EngineEvent::Error { run: None, message: warn });
    }

    // A store that fails to open must not take the app down: the user can still hold a
    // conversation, it just won't survive a restart. Losing history is bad; refusing to run at
    // all because of it is worse.
    let store = match Store::open_default().await {
        Ok(store) => Some(store),
        Err(e) => {
            tracing::error!(%e, "persistence disabled; this session will not be saved");
            let _ = evt_tx.send(EngineEvent::Error {
                run: None,
                message: format!("History will not be saved: {e}"),
            });
            None
        }
    };

    let conv = ConvId::new();
    if let Some(store) = &store
        && let Err(e) = store.create_conversation(conv, "Session").await
    {
        tracing::error!(%e, "could not create conversation row");
    }

    // One client for every provider: connection pooling is what keeps ten concurrent agents from
    // opening ten TLS handshakes to the same host.
    //
    // The timeouts are not optional. A streaming response that stops arriving — a dropped TLS
    // connection, a proxy timing out an idle stream, a provider that simply stalls — leaves the
    // run parked on the next chunk *forever*, and all the user sees is a caret that never turns
    // into a reply. `read_timeout` is the one that matters: it bounds the gap between chunks
    // rather than the length of the response, so a model that thinks for two minutes is fine
    // while a stream that dies mid-flight fails and says so.
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(120))
        // Detects the half-open connection that produces this symptom in the first place: the
        // socket looks ESTABLISHED to us long after the other end is gone.
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|e| {
            tracing::error!(%e, "could not build the http client with timeouts");
            reqwest::Client::new()
        });

    // Not fatal when absent. A first run has no providers yet, and the app has to come up so the
    // user can add one — refusing to start is how a settings screen becomes unreachable.
    let mut active: Option<Active> = match resolve_provider(&http).await {
        Ok(Some(found)) => Some(found),
        Ok(None) => {
            tracing::info!("no provider configured yet");
            None
        }
        Err(e) => {
            tracing::error!(%e, "could not load the configured provider");
            let _ = evt_tx.send(EngineEvent::Error { run: None, message: e });
            None
        }
    };
    if let Some((provider, model, budget)) = &active {
        tracing::info!(model, base_url = %provider.base_url(), budget, "provider ready");
    }

    let registry = Arc::new(ToolRegistry::with_builtins());
    let history: Arc<Mutex<Vec<provider::Message>>> = Arc::default();
    let pending: PendingApprovals = Arc::default();
    let always_allowed: Arc<Mutex<Vec<String>>> = Arc::default();
    // Off at every start. Not persisted anywhere on purpose — see `SetAutoApprove`.
    let auto_approve = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The answer a run returns is only of interest to a *delegating* agent; a top-level run has
    // already streamed every word of it to the UI, so the handle is kept purely to abort it.
    let mut runs: HashMap<RunId, JoinHandle<String>> = HashMap::new();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::SendUserMessage { text, max_agents, .. } => {
                let max_agents = max_agents.min(MAX_AGENTS);
                let Some((provider, model, budget)) = active.clone() else {
                    let _ = evt_tx
                        .send(EngineEvent::Error { run: None, message: NO_PROVIDER.into() });
                    continue;
                };

                // Attachments go in front of the question: the model reads the material, then
                // what to do with it. Resolved once, here, so the transcript records what was
                // actually sent rather than a path whose contents have since changed.
                let message = match context::attach(&sandbox.ctx.policy, &sandbox.root, &text).await
                {
                    Some(attached) => provider::Message {
                        role: provider::Role::User,
                        content: vec![
                            provider::Part::Text(attached),
                            provider::Part::Text(text),
                        ],
                    },
                    None => provider::Message::user(text),
                };
                if let Some(store) = &store
                    && let Err(e) = store.append_message(conv, &message).await
                {
                    tracing::error!(%e, "could not persist user message");
                }
                history.lock().unwrap().push(message);

                let run = RunId::new();
                // With no agents allowed, `delegate` is not merely refused — it is never
                // advertised. Offering a tool that always fails wastes a turn and reads to the
                // model as a broken environment.
                let granted =
                    if max_agents == 0 { Permission::CODING } else { TOP_LEVEL };
                let system = format!("{}\n\n{}", sandbox.system, agent_budget_prompt(max_agents));

                // Built per run, not per workspace: it carries the provider and the model in
                // force right now, so a settings change between turns lands on the next
                // delegation too.
                let delegator = Arc::new(SubAgents {
                    provider: Arc::clone(&provider),
                    model: model.clone(),
                    // The *base* prompt: every run below composes its own allowance onto it from
                    // what the pool has left.
                    system: sandbox.system.clone(),
                    registry: Arc::clone(&registry),
                    policy: sandbox.ctx.policy.clone(),
                    cwd: sandbox.ctx.cwd.clone(),
                    store: store.clone(),
                    events: evt_tx.clone(),
                    pending: Arc::clone(&pending),
                    always_allowed: Arc::clone(&always_allowed),
                    auto_approve: Arc::clone(&auto_approve),
                    granted,
                    budget,
                    // One pool per task, created here and shared down the tree.
                    pool: AgentPool::new(max_agents),
                    max_agents,
                });
                let ctx = Arc::new(ToolCtx {
                    policy: sandbox.ctx.policy.clone(),
                    cwd: sandbox.ctx.cwd.clone(),
                    depth: 0,
                    delegate: Some(delegator),
                });

                let agent = AgentRun {
                    run,
                    conv,
                    store: store.clone(),
                    provider,
                    model,
                    // Snapshotted per run. A workspace switch mid-run leaves this one on the
                    // boundary it was told about and started working inside.
                    system,
                    registry: Arc::clone(&registry),
                    ctx,
                    granted,
                    budget,
                    history: Arc::clone(&history),
                    pending: Arc::clone(&pending),
                    events: evt_tx.clone(),
                    always_allowed: Arc::clone(&always_allowed),
                    auto_approve: Arc::clone(&auto_approve),
                    quiet: false,
                    // The agent the user is addressing needs no label; every prompt they have
                    // ever seen came from it.
                    agent: None,
                };
                runs.insert(run, tokio::spawn(agent.execute()));
            }

            EngineCommand::ResolveApproval { call, decision } => {
                // Dropping the sender instead would read as a denial; sending is what unblocks
                // the parked run.
                if let Some(tx) = pending.lock().unwrap().remove(&call) {
                    tracing::info!(%call, ?decision, "user resolved approval");
                    let _ = tx.send(decision);
                } else {
                    tracing::warn!(%call, "approval for an unknown or already-resolved call");
                }
            }

            EngineCommand::SetAutoApprove(on) => {
                auto_approve.store(on, std::sync::atomic::Ordering::SeqCst);
                // Recorded at warn level in both directions: the log has to show when the
                // prompts stopped and when they came back, or an unattended write cannot be
                // placed in time afterwards.
                tracing::warn!(on, "auto-approve mode changed");
                if let Some(store) = &store
                    && let Err(e) = store
                        .audit(store::AuditEntry {
                            action: "approval",
                            tool: None,
                            detail: Some(if on {
                                "auto-approve turned ON — tool calls run without asking"
                            } else {
                                "auto-approve turned off — tool calls prompt again"
                            }),
                            approved: None,
                            unattended: on,
                            elevated: false,
                            ok: Some(true),
                        })
                        .await
                {
                    tracing::error!(%e, "could not audit the auto-approve change");
                }
                let _ = evt_tx.send(EngineEvent::AutoApprove(on));
            }

            EngineCommand::RequestProviders => send_providers(&evt_tx).await,

            EngineCommand::SaveProvider { entry, key } => {
                let name = entry.name.clone();
                let result = edit_config(move |cfg| {
                    // The key goes in first: a config entry pointing at a key that was never
                    // stored fails at request time with a confusing 401, whereas a stored key
                    // with no entry is inert and gets overwritten by the next save.
                    if let Some(key) = &key {
                        secrets::set(&entry.name, key).map_err(|e| e.to_string())?;
                    }
                    match cfg.providers.iter_mut().find(|p| p.name == entry.name) {
                        Some(slot) => *slot = entry.clone(),
                        None => cfg.providers.push(entry.clone()),
                    }
                    cfg.active_provider = Some(entry.name);
                    Ok(())
                })
                .await;

                match result {
                    Ok(()) => {
                        tracing::info!(provider = %name, "provider saved");
                        reload(&http, &mut active, &evt_tx).await;
                    }
                    Err(e) => {
                        tracing::error!(provider = %name, %e, "could not save provider");
                        let _ = evt_tx.send(EngineEvent::Error {
                            run: None,
                            message: format!("Could not save `{name}`: {e}"),
                        });
                    }
                }
                send_providers(&evt_tx).await;
            }

            EngineCommand::DeleteProvider { name } => {
                let result = edit_config({
                    let name = name.clone();
                    move |cfg| {
                        cfg.providers.retain(|p| p.name != name);
                        if cfg.active_provider.as_deref() == Some(name.as_str()) {
                            cfg.active_provider = None;
                        }
                        // Removing the row but leaving the key would leave a live credential on
                        // the machine that the user believes they just deleted.
                        secrets::delete(&name).map_err(|e| e.to_string())
                    }
                })
                .await;

                match result {
                    Ok(()) => {
                        tracing::info!(provider = %name, "provider deleted");
                        reload(&http, &mut active, &evt_tx).await;
                    }
                    Err(e) => {
                        tracing::error!(provider = %name, %e, "could not delete provider");
                        let _ = evt_tx.send(EngineEvent::Error {
                            run: None,
                            message: format!(
                                "Could not fully delete `{name}`: {e}. The stored key may still \
                                 exist — check your keychain."
                            ),
                        });
                    }
                }
                send_providers(&evt_tx).await;
            }

            EngineCommand::SetActiveProvider { name } => {
                let result = edit_config({
                    let name = name.clone();
                    move |cfg| {
                        if !cfg.providers.iter().any(|p| p.name == name) {
                            return Err(format!("no provider named `{name}`"));
                        }
                        cfg.active_provider = Some(name);
                        Ok(())
                    }
                })
                .await;

                match result {
                    Ok(()) => reload(&http, &mut active, &evt_tx).await,
                    Err(e) => {
                        let _ = evt_tx
                            .send(EngineEvent::Error { run: None, message: e });
                    }
                }
                send_providers(&evt_tx).await;
            }

            EngineCommand::ListModels { entry, key } => {
                let (http, evt_tx) = (http.clone(), evt_tx.clone());
                // Spawned, not awaited: an unreachable endpoint takes the full connect timeout,
                // and the user must still be able to cancel a run or fix the URL meanwhile.
                tokio::spawn(async move {
                    match fetch_models(&http, entry, key).await {
                        Ok((provider, models)) => {
                            tracing::info!(%provider, count = models.len(), "listed models");
                            let _ = evt_tx.send(EngineEvent::Models { provider, models });
                        }
                        Err(message) => {
                            tracing::error!(%message, "could not list models");
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message });
                        }
                    }
                });
            }

            EngineCommand::RequestWorkspaces => send_workspaces(&evt_tx, &sandbox.root).await,

            EngineCommand::SetWorkspace { root } => {
                match save_workspace(root).await {
                    Ok(canonical) => {
                        let (next, warn) = Sandbox::open(Some(canonical));
                        if let Some(warn) = warn {
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message: warn });
                        }
                        tracing::info!(root = %next.root.display(), "workspace switched");
                        sandbox = next;
                    }
                    Err(e) => {
                        tracing::error!(%e, "could not open workspace");
                        let _ = evt_tx.send(EngineEvent::Error {
                            run: None,
                            message: format!("Could not open that workspace: {e}"),
                        });
                    }
                }
                send_workspaces(&evt_tx, &sandbox.root).await;
            }

            EngineCommand::ForgetWorkspace { root } => {
                let was_active = sandbox.root == root;
                let result = edit_config({
                    let root = root.clone();
                    move |cfg| {
                        cfg.workspaces.retain(|w| w != &root);
                        if cfg.active_workspace.as_deref() == Some(root.as_path()) {
                            cfg.active_workspace = None;
                        }
                        Ok(())
                    }
                })
                .await;

                match result {
                    Ok(()) => {
                        tracing::info!(root = %root.display(), "workspace forgotten");
                        // Narrowed immediately when it was the active one: continuing to grant
                        // access to a directory the user has just disowned is the one outcome
                        // this command must not produce.
                        if was_active {
                            (sandbox, _) = Sandbox::open(None);
                            tracing::info!(
                                root = %sandbox.root.display(),
                                "fell back to the working directory",
                            );
                        }
                    }
                    Err(e) => {
                        tracing::error!(root = %root.display(), %e, "could not forget workspace");
                        let _ = evt_tx.send(EngineEvent::Error {
                            run: None,
                            message: format!("Could not update the workspace list: {e}"),
                        });
                    }
                }
                send_workspaces(&evt_tx, &sandbox.root).await;
            }

            // All three are spawned rather than awaited: a listing on a cold disk, or a save to a
            // network mount, must not hold up an approval the user is waiting to answer.
            EngineCommand::ListDir { path } => {
                let (policy, evt_tx) = (sandbox.ctx.policy.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    match files::list(&policy, &path).await {
                        Ok((entries, truncated)) => {
                            let _ = evt_tx.send(EngineEvent::DirListing { path, entries, truncated });
                        }
                        Err(message) => {
                            tracing::warn!(path = %path.display(), %message, "could not list");
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message });
                        }
                    }
                });
            }

            EngineCommand::OpenFile { path } => {
                let (policy, evt_tx) = (sandbox.ctx.policy.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    match files::read(&policy, &path).await {
                        Ok(content) => {
                            let _ = evt_tx.send(EngineEvent::FileOpened { path, content });
                        }
                        Err(message) => {
                            tracing::warn!(path = %path.display(), %message, "could not open");
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message });
                        }
                    }
                });
            }

            EngineCommand::Search { query } => {
                let (policy, root, evt_tx) =
                    (sandbox.ctx.policy.clone(), sandbox.root.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    let text = query.text.clone();
                    match search::search(&policy, &root, query).await {
                        Ok(results) => {
                            tracing::info!(
                                query = %text,
                                hits = results.hits.len(),
                                files = results.files_searched,
                                "search finished",
                            );
                            let _ =
                                evt_tx.send(EngineEvent::SearchResults { query: text, results });
                        }
                        Err(message) => {
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message });
                        }
                    }
                });
            }

            EngineCommand::RequestWorkspaceFiles => {
                let (policy, root, evt_tx) =
                    (sandbox.ctx.policy.clone(), sandbox.root.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    match search::paths(&policy, &root).await {
                        Ok(files) => {
                            let _ = evt_tx.send(EngineEvent::WorkspaceFiles { files });
                        }
                        // Logged, not shown: this answers a keystroke the user did not ask a
                        // question with, and a modal about it would be noise. The completion
                        // list simply stays as it was.
                        Err(e) => tracing::warn!(%e, "could not list workspace files"),
                    }
                });
            }

            EngineCommand::Replace { query, replacement } => {
                let (policy, root, evt_tx) =
                    (sandbox.ctx.policy.clone(), sandbox.root.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    let (text, regex) = (query.text.clone(), query.regex);
                    match search::replace(&policy, &root, query, replacement).await {
                        Ok((files, replacements)) => {
                            // Logged unconditionally: this rewrites files in bulk with no
                            // per-file approval, so the audit trail is the only record of what
                            // happened if the result is not what the user expected.
                            tracing::info!(
                                query = %text,
                                regex,
                                files,
                                replacements,
                                "replace finished",
                            );
                            let _ = evt_tx.send(EngineEvent::Replaced { files, replacements });
                        }
                        Err(message) => {
                            tracing::error!(query = %text, %message, "replace failed");
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message });
                        }
                    }
                });
            }

            EngineCommand::SaveFile { path, content } => {
                let (policy, evt_tx) = (sandbox.ctx.policy.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    match files::write(&policy, &path, content).await {
                        Ok(()) => {
                            tracing::info!(path = %path.display(), "saved");
                            let _ = evt_tx.send(EngineEvent::FileSaved { path });
                        }
                        Err(message) => {
                            // Loud, and *not* followed by a FileSaved: the tab keeps its modified
                            // marker so the user knows the work is still only in memory.
                            tracing::error!(path = %path.display(), %message, "could not save");
                            let _ = evt_tx.send(EngineEvent::Error {
                                run: None,
                                message: format!("Not saved — {message}"),
                            });
                        }
                    }
                });
            }

            EngineCommand::CancelRun(id) => {
                if let Some(task) = runs.remove(&id) {
                    // ponytail: abort is enough while tools are individually short-lived. Swap
                    // to a CancellationToken when a tool needs to unwind (e.g. kill a child
                    // process group) rather than just stop being polled.
                    task.abort();
                    let _ = evt_tx.send(EngineEvent::RunFinished {
                        run: id,
                        stop: StopReason::Cancelled,
                        usage: Usage::default(),
                    });
                }
            }

            EngineCommand::Shutdown => break,
        }

        runs.retain(|_, t| !t.is_finished());
    }

    for (_, task) in runs {
        task.abort();
    }
    tracing::info!("engine stopped");
}

/// Re-resolve the active provider after a settings change, reporting failure to the UI.
///
/// A save that leaves the engine pointing at the *old* provider is the worst outcome here: the
/// settings screen would show the change while requests kept going somewhere else.
async fn reload(
    http: &reqwest::Client,
    active: &mut Option<Active>,
    evt_tx: &broadcast::Sender<EngineEvent>,
) {
    match resolve_provider(http).await {
        Ok(found) => {
            if let Some((provider, model, budget)) = &found {
                tracing::info!(model, base_url = %provider.base_url(), budget, "provider reloaded");
            }
            *active = found;
        }
        Err(e) => {
            tracing::error!(%e, "could not load the configured provider");
            let _ = evt_tx.send(EngineEvent::Error { run: None, message: e });
            // Cleared, not left stale: a provider the user has just edited into an unusable
            // state must stop serving requests rather than silently keep using the old settings.
            *active = None;
        }
    }
}

/// Load, mutate, save.
///
/// Runs on the blocking pool: both halves are synchronous IO, and the keychain half can block for
/// seconds when the user's keyring is locked and the platform puts up an unlock prompt.
async fn edit_config(
    f: impl FnOnce(&mut Config) -> Result<(), String> + Send + 'static,
) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        let mut cfg = Config::load().map_err(|e| e.to_string())?;
        f(&mut cfg)?;
        cfg.save().map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Expand a leading `~`.
///
/// A GUI text field gets no shell expansion, and `~/code/thing` is what people type. Without this
/// it resolves against the working directory as a literal directory named `~` and fails with a
/// "no such file" that points at a path the user never wrote.
///
/// Public so the settings UI can check a typed path against the same rule the engine will apply.
/// That check is for the *message* only — the engine still validates, because the UI is not a
/// trust boundary and a path can stop being a directory between the click and the switch.
pub fn expand_home(path: &Path) -> PathBuf {
    let Ok(rest) = path.strip_prefix("~") else { return path.to_path_buf() };
    match directories::UserDirs::new() {
        Some(dirs) => dirs.home_dir().join(rest),
        None => path.to_path_buf(),
    }
}

/// Validate a workspace root, persist it, and return the canonical form.
///
/// Canonicalised *before* saving because the policy compares resolved paths: a root reached
/// through a symlink would never prefix-match the resolved paths of the files inside it, and
/// every tool call would be denied for a directory the user can plainly see.
async fn save_workspace(root: PathBuf) -> Result<PathBuf, String> {
    let canonical = tokio::task::spawn_blocking(move || {
        let expanded = expand_home(&root);
        let canonical =
            expanded.canonicalize().map_err(|e| format!("{}: {e}", expanded.display()))?;
        // Rejected here rather than at first use: a file passes `canonicalize` happily, and a
        // sandbox rooted at one denies everything with a message about the workspace.
        if !canonical.is_dir() {
            return Err(format!("{} is not a directory", canonical.display()));
        }
        Ok::<PathBuf, String>(canonical)
    })
    .await
    .map_err(|e| e.to_string())??;

    let saved = canonical.clone();
    edit_config(move |cfg| {
        if !cfg.workspaces.contains(&saved) {
            cfg.workspaces.push(saved.clone());
        }
        cfg.active_workspace = Some(saved);
        Ok(())
    })
    .await?;
    Ok(canonical)
}

async fn send_workspaces(evt_tx: &broadcast::Sender<EngineEvent>, active: &Path) {
    let saved = match tokio::task::spawn_blocking(Config::load).await {
        Ok(Ok(cfg)) => cfg.workspaces,
        // The list is a convenience; the *active* root is the security-relevant half and is held
        // in memory, so a failure here degrades to "no saved workspaces" rather than to no answer.
        Ok(Err(e)) => {
            tracing::error!(%e, "could not read the workspace list");
            Vec::new()
        }
        Err(e) => {
            tracing::error!(%e, "workspace list task failed");
            Vec::new()
        }
    };
    let _ = evt_tx.send(EngineEvent::Workspaces { saved, active: active.to_path_buf() });
}

async fn send_providers(evt_tx: &broadcast::Sender<EngineEvent>) {
    match provider_summaries().await {
        Ok((providers, keychain)) => {
            let _ = evt_tx.send(EngineEvent::Providers { providers, keychain });
        }
        Err(e) => {
            let _ = evt_tx.send(EngineEvent::Error {
                run: None,
                message: format!("Could not read the provider list: {e}"),
            });
        }
    }
}

/// The provider list as the settings UI sees it — never including a key.
async fn provider_summaries() -> Result<(Vec<ProviderSummary>, bool), String> {
    tokio::task::spawn_blocking(|| {
        let cfg = Config::load().map_err(|e| e.to_string())?;
        let keychain = secrets::available();
        let active = cfg.active().map(|e| e.name.clone());

        let providers = cfg
            .providers
            .into_iter()
            .map(|entry| ProviderSummary {
                // Asks only *whether* a key exists. The value is read at request time and has no
                // route back across the channel.
                has_key: matches!(secrets::get(&entry.name), Ok(Some(_))),
                active: active.as_deref() == Some(entry.name.as_str()),
                entry,
            })
            .collect();
        Ok((providers, keychain))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Ask a provider which models the key can reach.
///
/// Deliberately does not touch the saved config: the settings form calls this for a provider the
/// user is still typing, and listing models must not be a side-effectful save.
async fn fetch_models(
    http: &reqwest::Client,
    entry: ProviderEntry,
    typed: Option<SecretString>,
) -> Result<(String, Vec<provider::ModelInfo>), String> {
    let name = entry.name.clone();

    // A key typed into the form wins; otherwise fall back to the stored one, so listing models
    // for an already-saved provider does not mean retyping its key.
    let api_key = match typed {
        Some(key) => Some(key),
        None if entry.needs_key => {
            let account = name.clone();
            tokio::task::spawn_blocking(move || secrets::get(&account))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?
        }
        None => None,
    };

    if entry.needs_key && api_key.is_none() {
        return Err(format!("`{name}` needs an API key before its models can be listed."));
    }

    let cfg = ProviderConfig {
        id: ProviderId::new(),
        name: name.clone(),
        kind: entry.kind,
        base_url: entry.base_url,
        api_key,
        org_id: entry.org_id,
        headers: entry.headers,
    };
    let provider = AnyProvider::new(cfg, http.clone()).map_err(|e| e.to_string())?;
    let models = provider.models().await.map_err(|e| format!("listing models: {e}"))?;

    if models.is_empty() {
        return Err(format!("`{name}` returned no models."));
    }
    Ok((name, models))
}

/// Pick a provider: the configured active entry, else env vars.
///
/// `Ok(None)` means nothing is configured — a normal first run. `Err` means something *is*
/// configured and is broken, which the user needs told about.
async fn resolve_provider(http: &reqwest::Client) -> Result<Option<Active>, String> {
    let http = http.clone();
    tokio::task::spawn_blocking(move || {
        // An explicit env override wins, so development doesn't require editing the config file.
        if std::env::var_os("MESHFLOW_BASE_URL").is_some()
            || std::env::var_os("MESHFLOW_API_KEY").is_some()
        {
            return provider_from_env(&http);
        }

        // A malformed config is an error, not a silent fallback: the user's providers are in
        // there, and quietly running on env vars instead hides that the file needs fixing.
        let cfg = Config::load().map_err(|e| e.to_string())?;
        match cfg.active() {
            Some(entry) => provider_from_config(entry, &http).map(Some),
            None => provider_from_env(&http),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Build a provider from a config entry, pulling its key from the OS keychain.
fn provider_from_config(entry: &ProviderEntry, http: &reqwest::Client) -> Result<Active, String> {
    let api_key = if entry.needs_key {
        match secrets::get(&entry.name) {
            Ok(Some(key)) => Some(key),
            Ok(None) => {
                return Err(format!(
                    "`{}` expects an API key but none is stored. Open Settings and paste one in.",
                    entry.name
                ));
            }
            Err(e) => return Err(format!("could not read the keychain for `{}`: {e}", entry.name)),
        }
    } else {
        None
    };

    let cfg = ProviderConfig {
        id: ProviderId::new(),
        name: entry.name.clone(),
        kind: entry.kind,
        base_url: entry.base_url.clone(),
        api_key,
        org_id: entry.org_id.clone(),
        headers: entry.headers.clone(),
    };
    let provider = AnyProvider::new(cfg, http.clone()).map_err(|e| e.to_string())?;
    Ok((Arc::new(provider), entry.model.clone(), budget_for(entry.context_window)))
}

/// Development override. Pointing `MESHFLOW_BASE_URL` at Ollama or LM Studio works with no key.
fn provider_from_env(http: &reqwest::Client) -> Result<Option<Active>, String> {
    let base_url =
        std::env::var("MESHFLOW_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
    let model = std::env::var("MESHFLOW_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
    let api_key = std::env::var("MESHFLOW_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .ok()
        .map(SecretString::from);

    // A local endpoint legitimately has no key; a remote one without a key would just 401.
    let is_local = base_url.contains("localhost") || base_url.contains("127.0.0.1");
    if api_key.is_none() && !is_local {
        return Ok(None);
    }

    let cfg = ProviderConfig {
        id: ProviderId::new(),
        name: "default".into(),
        kind: ProviderKind::OpenAi,
        base_url,
        api_key,
        org_id: std::env::var("OPENAI_ORG_ID").ok(),
        headers: Default::default(),
    };
    let provider = AnyProvider::new(cfg, http.clone()).map_err(|e| e.to_string())?;
    // The env override names a model but never a window — whatever it points at gets the
    // conservative default, which is the right guess for the Ollama endpoint it usually is.
    Ok(Some((Arc::new(provider), model, budget_for(None))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsaccess::Op;

    #[test]
    fn the_agent_pool_hands_out_exactly_what_it_was_given() {
        let pool = AgentPool::new(2);
        assert!(pool.take());
        assert!(pool.take());
        assert!(!pool.take(), "a third agent came out of a pool of two");
    }

    #[test]
    fn the_pool_is_shared_by_the_whole_tree_not_refilled_per_level() {
        // What "at most 2" has to mean: two agents for the task, not two per level — which
        // would be 2, then 4, then 8 as they nest.
        let pool = AgentPool::new(2);
        let child = pool.clone();
        assert!(pool.take());
        assert!(child.take(), "the child draws on the same pool");
        assert!(!child.take());
        assert!(!pool.take(), "the parent sees the child's spending");
    }

    #[test]
    fn the_last_agent_out_of_the_pool_is_not_given_the_power_to_delegate() {
        // What the transcript showed: with a cap of 1, the sub-agent kept `delegate` and spent a
        // turn being told the pool was empty.
        let pool = AgentPool::new(1);
        assert!(pool.take());
        assert_eq!(pool.remaining(), 0);

        let inheritable = match pool.remaining() {
            0 => Permission::CODING,
            _ => Permission::CODING | Permission::AGENT,
        };
        assert!(!TOP_LEVEL.delegated(inheritable).contains(Permission::AGENT));

        // With room to spare it is still passed down.
        let roomy = AgentPool::new(3);
        assert!(roomy.take());
        assert!(roomy.remaining() > 0);
        assert!(
            TOP_LEVEL
                .delegated(Permission::CODING | Permission::AGENT)
                .contains(Permission::AGENT),
        );
    }

    #[test]
    fn a_pool_of_zero_never_hands_one_out() {
        assert!(!AgentPool::new(0).take());
        // And the ceiling holds whatever the command asked for.
        let huge = AgentPool::new(u8::MAX);
        for _ in 0..MAX_AGENTS {
            assert!(huge.take());
        }
        assert!(!huge.take(), "the engine's own ceiling is what counts");
    }

    #[test]
    fn the_budget_prompt_states_the_number_the_refusal_will_use() {
        assert!(agent_budget_prompt(0).contains("no sub-agents"));
        assert!(agent_budget_prompt(3).contains('3'));
        // A ceiling has to read as one, or the model treats it as work to be done.
        assert!(agent_budget_prompt(3).contains("ceiling, not a target"));
    }

    #[test]
    fn a_workspace_root_bounds_the_sandbox() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(tmp.path().join("outside.txt"), "not yours").unwrap();

        let (sandbox, warn) = Sandbox::open(Some(root.clone()));
        assert!(warn.is_none());
        assert_eq!(sandbox.ctx.policy.mode(), AccessMode::WorkspaceSandbox);
        assert!(sandbox.ctx.policy.check(&root.join("ok.txt"), Op::Write).is_ok());
        assert!(sandbox.ctx.policy.check(&tmp.path().join("outside.txt"), Op::Read).is_err());

        // Layer 1 of the instruction hierarchy has to name the live root, or the model learns
        // its boundary from a denial instead of from the prompt.
        let resolved = root.canonicalize().unwrap();
        assert!(
            sandbox.system.contains(&resolved.display().to_string()),
            "system prompt does not state the workspace: {}",
            sandbox.system
        );
    }

    #[test]
    fn a_root_that_does_not_resolve_warns_and_falls_back() {
        let (sandbox, warn) = Sandbox::open(Some(PathBuf::from("/definitely/not/here")));

        // The failure mode this guards: `PathPolicy` silently drops roots it cannot canonicalise,
        // so accepting one would leave *no* roots — denying every path with a message about the
        // workspace rather than about the missing directory.
        assert!(warn.is_some(), "a missing workspace must say so");
        assert_eq!(sandbox.ctx.policy.mode(), AccessMode::Cwd);
        assert_eq!(sandbox.ctx.policy.roots().len(), 1);
        assert_eq!(sandbox.root, sandbox.ctx.cwd);
    }

    #[test]
    fn a_file_is_not_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "hello").unwrap();

        // `canonicalize` succeeds on a file, so without the explicit directory check this would
        // be accepted and then deny everything inside the "workspace".
        let (sandbox, warn) = Sandbox::open(Some(file));
        assert!(warn.is_some());
        assert_eq!(sandbox.ctx.policy.mode(), AccessMode::Cwd);
    }

    #[test]
    fn a_leading_tilde_expands_to_the_home_directory() {
        let home = directories::UserDirs::new().map(|d| d.home_dir().to_path_buf());
        if let Some(home) = home {
            assert_eq!(expand_home(Path::new("~/code")), home.join("code"));
            assert_eq!(expand_home(Path::new("~")), home);
        }
        // Only a *leading* `~` is a home reference; one mid-path is a real directory name.
        assert_eq!(expand_home(Path::new("/srv/~/x")), PathBuf::from("/srv/~/x"));
        assert_eq!(expand_home(Path::new("/srv/proj")), PathBuf::from("/srv/proj"));
    }
}
