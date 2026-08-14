//! MeshFlow engine.
//!
//! Runs on a multi-threaded Tokio runtime. Owns providers, agents, tools and storage.
//! Knows nothing about the UI beyond [`proto`] — deliberately, because Freya's reactivity is
//! single-threaded and `!Send`, so the two worlds can never share state directly.

pub mod agent;
pub mod config;
pub mod diff;
pub mod files;
pub mod fsaccess;
pub mod logging;
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
use tool::{Permission, ToolCtx, ToolRegistry};

/// What the UI is told when a message is sent with nothing configured. Points at the fix rather
/// than just naming the problem — this is the first thing a new user sees.
const NO_PROVIDER: &str =
    "No AI provider configured. Open Settings and add one, or set MESHFLOW_API_KEY.";

/// A resolved provider and the model to talk to.
type Active = (Arc<AnyProvider>, String);

/// Layer 1 of the instruction hierarchy. The live path policy is appended at run time so the
/// model learns its limits from the prompt rather than from a failed tool call.
const SYSTEM_PROMPT: &str = "You are MeshFlow, a local AI coding assistant. Be concise. \
     Use markdown, and fenced code blocks with a language tag for code. \
     Use the provided tools to inspect and modify files rather than guessing at their contents.";

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
        let ctx = Arc::new(ToolCtx { cwd: root.clone(), policy, depth: 0 });
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
    let http = reqwest::Client::new();

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
    if let Some((provider, model)) = &active {
        tracing::info!(model, base_url = %provider.base_url(), "provider ready");
    }

    let registry = Arc::new(ToolRegistry::with_builtins());
    let history: Arc<Mutex<Vec<provider::Message>>> = Arc::default();
    let pending: PendingApprovals = Arc::default();
    let always_allowed: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut runs: HashMap<RunId, JoinHandle<()>> = HashMap::new();

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            EngineCommand::SendUserMessage { text, .. } => {
                let Some((provider, model)) = active.clone() else {
                    let _ = evt_tx
                        .send(EngineEvent::Error { run: None, message: NO_PROVIDER.into() });
                    continue;
                };

                let message = provider::Message::user(text);
                if let Some(store) = &store
                    && let Err(e) = store.append_message(conv, &message).await
                {
                    tracing::error!(%e, "could not persist user message");
                }
                history.lock().unwrap().push(message);

                let run = RunId::new();
                let agent = AgentRun {
                    run,
                    conv,
                    store: store.clone(),
                    provider,
                    model,
                    // Snapshotted per run. A workspace switch mid-run leaves this one on the
                    // boundary it was told about and started working inside.
                    system: sandbox.system.clone(),
                    registry: Arc::clone(&registry),
                    ctx: Arc::clone(&sandbox.ctx),
                    granted: Permission::CODING,
                    history: Arc::clone(&history),
                    pending: Arc::clone(&pending),
                    events: evt_tx.clone(),
                    always_allowed: Arc::clone(&always_allowed),
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

            EngineCommand::Search { query, case_sensitive } => {
                let (policy, root, evt_tx) =
                    (sandbox.ctx.policy.clone(), sandbox.root.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    match search::search(&policy, &root, query.clone(), case_sensitive).await {
                        Ok(results) => {
                            tracing::info!(
                                %query,
                                hits = results.hits.len(),
                                files = results.files_searched,
                                "search finished",
                            );
                            let _ = evt_tx.send(EngineEvent::SearchResults { query, results });
                        }
                        Err(message) => {
                            let _ = evt_tx.send(EngineEvent::Error { run: None, message });
                        }
                    }
                });
            }

            EngineCommand::Replace { query, replacement, case_sensitive } => {
                let (policy, root, evt_tx) =
                    (sandbox.ctx.policy.clone(), sandbox.root.clone(), evt_tx.clone());
                tokio::spawn(async move {
                    match search::replace(&policy, &root, query.clone(), replacement, case_sensitive)
                        .await
                    {
                        Ok((files, replacements)) => {
                            // Logged unconditionally: this rewrites files in bulk with no
                            // per-file approval, so the audit trail is the only record of what
                            // happened if the result is not what the user expected.
                            tracing::info!(%query, files, replacements, "replace finished");
                            let _ = evt_tx.send(EngineEvent::Replaced { files, replacements });
                        }
                        Err(message) => {
                            tracing::error!(%query, %message, "replace failed");
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
            if let Some((provider, model)) = &found {
                tracing::info!(model, base_url = %provider.base_url(), "provider reloaded");
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
    Ok((Arc::new(provider), entry.model.clone()))
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
    Ok(Some((Arc::new(provider), model)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsaccess::Op;

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
