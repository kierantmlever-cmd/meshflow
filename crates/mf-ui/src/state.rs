//! Application state, hoisted to the root, and the single engine-event drain.
//!
//! Panels unmount when the user switches tabs, so nothing that has to survive a switch can live
//! inside one. The event drain especially: parked in `ChatView` it was dropped along with the
//! view, so switching to Settings mid-reply threw away the rest of the stream and the run never
//! appeared to finish.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Duration,
};

use freya::prelude::*;
use freya::terminal::TerminalHandle;
use mf_engine::{
    files::Entry,
    proto::{
        ConvId, EngineCommand, EngineEvent, LogRecord, ProviderSummary, RunId, StopReason,
        StreamEvent,
    },
    provider::ModelInfo,
};
use tokio::{sync::broadcast::error::RecvError, time::MissedTickBehavior};

use crate::{
    Bridge,
    approval::PendingApproval,
    chat::{Author, ChatMessage},
    files::Tab,
    search::SearchState,
};

/// How long deltas are allowed to pile up before the markdown is re-rendered.
///
/// ponytail: coalesced full re-parse. The cost is Freya rebuilding the element tree, not
/// pulldown-cmark. Go incremental only if long replies visibly stutter.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Log lines held for the viewer. The full history is on disk; this is a tail, and an unbounded
/// one would grow without limit across a long session.
const LOG_CAPACITY: usize = 1000;

#[derive(Clone, Copy)]
pub struct AppState {
    pub conv: ConvId,
    pub messages: State<Vec<ChatMessage>>,
    pub active_run: State<Option<RunId>>,
    pub approvals: State<Vec<PendingApproval>>,
    /// Newest first, so the viewer never needs to auto-scroll to show the latest line.
    pub logs: State<Vec<LogRecord>>,
    pub providers: State<Vec<ProviderSummary>>,
    /// Models most recently fetched from a provider, and which provider they came from.
    pub models: State<ModelList>,
    /// The last engine error, kept so a screen other than Chat can show what went wrong. A
    /// failed model fetch belongs next to the button that triggered it, not in the transcript.
    pub last_error: State<String>,
    /// False when no OS keychain is reachable, so settings can say so before a key is typed.
    pub keychain: State<bool>,
    /// The directory agents are sandboxed to. Shown in the header on every screen, because what
    /// the agent can reach is not something the user should have to go looking for.
    pub workspace: State<PathBuf>,
    /// Roots the user has saved. Not always containing [`Self::workspace`] — with none saved the
    /// effective root is the process working directory.
    pub workspaces: State<Vec<PathBuf>>,
    /// Spawned on first use and kept here so switching tabs doesn't kill the shell process.
    pub terminal: State<Option<TerminalHandle>>,
    /// Directory listings, keyed by directory. Cached so expanding a folder twice does not
    /// re-walk the disk and make the tree flicker.
    pub listings: State<HashMap<PathBuf, Vec<Entry>>>,
    /// Which directories are open in the tree.
    pub expanded: State<HashSet<PathBuf>>,
    /// Open editor buffers. Here rather than in the panel because the panel unmounts on every tab
    /// switch and would take the user's unsaved edits with it.
    pub tabs: State<Vec<Tab>>,
    pub active_tab: State<usize>,
    /// The search panel. At the root so a query survives a tab switch — re-walking the workspace
    /// because the user glanced at the terminal would be a slow way to lose their place.
    pub search: State<SearchState>,
}

/// A provider's model catalogue, tagged with whose it is — a list left over from the previously
/// selected provider would otherwise be offered as if it belonged to the current one.
#[derive(Clone, Default, PartialEq)]
pub struct ModelList {
    pub provider: String,
    pub models: Vec<ModelInfo>,
}

/// Build the state and start the drain. Call once, from the root component.
pub fn use_app_state(bridge: &Bridge) -> AppState {
    let state = AppState {
        conv: use_hook(ConvId::new),
        messages: use_state(Vec::new),
        active_run: use_state(|| None),
        approvals: use_state(Vec::new),
        logs: use_state(Vec::new),
        providers: use_state(Vec::new),
        models: use_state(ModelList::default),
        last_error: use_state(String::new),
        keychain: use_state(|| true),
        // The engine's own fallback, so the header is right before the first event arrives
        // rather than blank or wrong.
        workspace: use_state(|| std::env::current_dir().unwrap_or_default()),
        workspaces: use_state(Vec::new),
        terminal: use_state(|| None),
        listings: use_state(HashMap::new),
        expanded: use_state(HashSet::new),
        tabs: use_state(Vec::new),
        active_tab: use_state(|| 0),
        search: use_state(SearchState::default),
    };

    use_hook({
        let (evt_tx, cmd_tx) = (bridge.evt_tx.clone(), bridge.cmd_tx.clone());
        move || spawn(drain(evt_tx.subscribe(), state, cmd_tx))
    });

    // Asked for at startup, not when the workspace screen opens: the header shows the root on
    // every tab, and the terminal starts in it on first use.
    use_hook({
        let cmd_tx = bridge.cmd_tx.clone();
        move || {
            let _ = cmd_tx.send(EngineCommand::RequestWorkspaces);
        }
    });

    state
}

async fn drain(
    mut rx: tokio::sync::broadcast::Receiver<EngineEvent>,
    state: AppState,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<EngineCommand>,
) {
    let AppState {
        mut messages,
        mut active_run,
        mut approvals,
        mut logs,
        mut providers,
        mut models,
        mut last_error,
        mut keychain,
        mut workspace,
        mut workspaces,
        mut listings,
        mut expanded,
        mut tabs,
        mut active_tab,
        mut search,
        ..
    } = state;

    let mut pending = String::new();
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            received = rx.recv() => match received {
                Ok(EngineEvent::RunStarted { run, .. }) => {
                    active_run.set(Some(run));
                    messages.write().push(ChatMessage {
                        author: Author::Assistant,
                        text: String::new(),
                        streaming: true,
                    });
                }
                Ok(EngineEvent::Delta { event: StreamEvent::TextDelta(t), .. }) => {
                    pending.push_str(&t);
                }
                Ok(EngineEvent::ApprovalNeeded { call, tool, preview, .. }) => {
                    // Flush first so the user reads the text that led here before the modal
                    // covers it.
                    flush(&mut pending, messages);
                    approvals.write().push(PendingApproval { call, tool, preview });
                }
                Ok(EngineEvent::ToolStarted { tool, .. }) => {
                    flush(&mut pending, messages);
                    let mut w = messages.write();
                    if w.last().is_some_and(|m| m.streaming && m.text.is_empty()) {
                        w.pop();
                    }
                    w.push(ChatMessage {
                        author: Author::Tool,
                        text: format!("running `{tool}`…"),
                        streaming: true,
                    });
                }
                Ok(EngineEvent::ToolFinished { call, ok, summary, .. }) => {
                    approvals.write().retain(|a| a.call != call);
                    let mut w = messages.write();
                    if let Some(last) = w.last_mut()
                        && last.author == Author::Tool
                    {
                        last.streaming = false;
                        last.text = if ok { summary } else { format!("failed: {summary}") };
                    }
                    drop(w);
                    // The model keeps going after a tool, so open a fresh bubble for whatever it
                    // says next.
                    messages.write().push(ChatMessage {
                        author: Author::Assistant,
                        text: String::new(),
                        streaming: true,
                    });
                }
                Ok(EngineEvent::RunFinished { stop, .. }) => {
                    flush(&mut pending, messages);
                    let mut w = messages.write();
                    // A tool-only turn leaves an empty trailing bubble.
                    if w.last().is_some_and(|m| m.streaming && m.text.is_empty()) {
                        w.pop();
                    }
                    if let Some(last) = w.last_mut() {
                        last.streaming = false;
                        if stop == StopReason::Cancelled {
                            last.text.push_str("\n\n_cancelled_");
                        }
                    }
                    drop(w);
                    // A cancelled run never answers its prompts; leaving them up would let the
                    // user approve a run that no longer exists.
                    approvals.write().clear();
                    active_run.set(None);
                }
                Ok(EngineEvent::Error { message, .. }) => {
                    // Recorded as well as shown, so whichever screen the user is on can surface
                    // it. Settings errors are useless if they only ever land in the transcript.
                    last_error.set(message.clone());
                    flush(&mut pending, messages);
                    let mut w = messages.write();
                    // Drop the empty placeholder rather than leaving a blank bubble.
                    if w.last().is_some_and(|m| m.streaming && m.text.is_empty()) {
                        w.pop();
                    }
                    w.push(ChatMessage {
                        author: Author::Error,
                        text: message,
                        streaming: false,
                    });
                    drop(w);
                    active_run.set(None);
                }
                Ok(EngineEvent::Providers { providers: list, keychain: available }) => {
                    providers.set(list);
                    keychain.set(available);
                }
                Ok(EngineEvent::Models { provider, models: list }) => {
                    last_error.set(String::new());
                    models.set(ModelList { provider, models: list });
                }
                Ok(EngineEvent::Workspaces { saved, active }) => {
                    if *workspace.read() != active {
                        // The cached tree belongs to the directory we have just left. Open tabs
                        // deliberately stay: a file outside the new root can no longer be saved,
                        // but closing it would throw away unsaved edits over a workspace switch
                        // the user never connected to their work. A refused save says why.
                        listings.write().clear();
                        expanded.write().clear();
                    }
                    workspace.set(active);
                    workspaces.set(saved);
                }
                Ok(EngineEvent::DirListing { path, entries, truncated }) => {
                    if truncated {
                        tracing::warn!(
                            path = %path.display(),
                            "directory too large to show in full",
                        );
                    }
                    listings.write().insert(path, entries);
                }
                Ok(EngineEvent::FileOpened { path, content }) => {
                    let mut w = tabs.write();
                    // Re-checked on arrival, not just on click: two fast clicks both send an
                    // OpenFile before either reply lands, and a second buffer over the first
                    // would silently drop whatever was typed into it.
                    match w.iter().position(|t: &Tab| t.path == path) {
                        Some(i) => active_tab.set(i),
                        None => {
                            w.push(Tab::open(path, &content));
                            active_tab.set(w.len() - 1);
                        }
                    }
                }
                Ok(EngineEvent::SearchResults { query, results }) => {
                    // Dropped if the query has moved on: a slow search finishing after the user
                    // retyped would replace the newer results with older ones.
                    if search.read().query == query {
                        search.write().results = results;
                    }
                }
                Ok(EngineEvent::Replaced { files, replacements }) => {
                    let mut w = search.write();
                    w.notice = format!(
                        "Replaced {replacements} matches across {files} files.",
                    );
                    // Every hit it was showing now points at text that is gone, so the list must
                    // not be left standing as if it were still accurate.
                    w.results = Default::default();
                    let (query, case_sensitive) = (w.query.clone(), w.case_sensitive);
                    drop(w);
                    let _ = cmd_tx.send(EngineCommand::Search { query, case_sensitive });
                }
                Ok(EngineEvent::FileSaved { path }) => {
                    if let Some(mut data) =
                        tabs.read().iter().find(|t| t.path == path).map(|t| t.data)
                    {
                        data.write().mark_as_saved();
                    }
                }
                Ok(EngineEvent::Log(record)) => {
                    let mut w = logs.write();
                    w.insert(0, record);
                    w.truncate(LOG_CAPACITY);
                }
                Ok(_) => {}
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "ui fell behind engine events");
                }
            },
            _ = ticker.tick() => flush(&mut pending, messages),
        }
    }
}

/// Append the buffered deltas to the message currently streaming.
fn flush(pending: &mut String, mut messages: State<Vec<ChatMessage>>) {
    if pending.is_empty() {
        return;
    }
    let mut w = messages.write();
    if let Some(last) = w.last_mut() {
        last.text.push_str(pending);
    }
    pending.clear();
}
