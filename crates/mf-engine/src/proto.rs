//! The only vocabulary shared between the engine world and the UI world.
//!
//! Everything here is `Send + Sync`. The UI sends [`EngineCommand`] down an unbounded mpsc and
//! receives [`EngineEvent`] from a broadcast. There is no other coupling in either direction —
//! deliberately, because Freya's reactivity is single-threaded and `!Send`, so the two worlds can
//! never share state directly.

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ProviderEntry;

macro_rules! id_type {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    )+};
}

id_type!(RunId, ConvId, AgentId, ToolCallId, ProviderId, WsId);

/// UI → engine. Fire and forget; every outcome comes back as an [`EngineEvent`].
#[derive(Debug, Clone)]
pub enum EngineCommand {
    SendUserMessage {
        conv: ConvId,
        text: String,
        /// Ceiling on how many sub-agents this one task may use, across the whole delegation
        /// tree. `0` turns delegation off, and the tool is not even offered.
        ///
        /// A ceiling, not a target — the agent is told the number and told to use only what the
        /// work needs. It rides with the message because it belongs to the task, not to the app:
        /// "summarise this file" and "port these six modules" want different answers, and the
        /// user sets it while looking at the thing they are about to send.
        max_agents: u8,
    },
    CancelRun(RunId),
    /// The user's answer to an [`EngineEvent::ApprovalNeeded`]. The run stays parked until
    /// this arrives — there is no timeout that silently proceeds.
    ResolveApproval {
        call: ToolCallId,
        decision: crate::tool::Approval,
    },

    /// Run tool calls that would need approval without asking for it.
    ///
    /// Off at startup, every time — it is deliberately not persisted. A mode that lets an agent
    /// overwrite files and run shell commands unattended should not be something a user turned on
    /// once, weeks ago, and has since forgotten about. Answered with
    /// [`EngineEvent::AutoApprove`], so what the UI shows is what the engine is actually doing.
    SetAutoApprove(bool),
    /// Ask for the provider list. Answered with [`EngineEvent::Providers`].
    RequestProviders,
    /// Create or replace a provider entry by name, and make it the active one.
    ///
    /// `key` goes straight to the OS keychain and never to `config.toml`. `None` leaves whatever
    /// key is already stored alone, so editing a model name does not require retyping the key.
    SaveProvider {
        entry: ProviderEntry,
        key: Option<SecretString>,
    },
    /// Remove the entry *and* its key. Dropping the config row alone would leave a credential on
    /// the machine that the user believes they deleted.
    DeleteProvider {
        name: String,
    },
    SetActiveProvider {
        name: String,
    },
    /// Ask the provider which models the user's key can actually reach.
    ///
    /// Takes the whole entry rather than a name because the settings form asks about a provider
    /// that may not be saved yet. `key` is the one typed into the form; when absent the engine
    /// falls back to the stored key, so listing models for a saved provider needs no retyping.
    ListModels {
        entry: ProviderEntry,
        key: Option<SecretString>,
    },

    /// Ask for the workspace list. Answered with [`EngineEvent::Workspaces`].
    RequestWorkspaces,
    /// Sandbox subsequent agent runs to `root`, and remember it.
    ///
    /// Takes effect on the *next* run. A run already in flight keeps the sandbox it started
    /// with: moving the boundary underneath a half-finished sequence of tool calls would fail
    /// them against a root the model was never told about, and the model would read that as the
    /// file being gone.
    SetWorkspace {
        root: std::path::PathBuf,
    },
    /// Forget a saved workspace. Only removes it from the list — never touches the directory.
    ForgetWorkspace {
        root: std::path::PathBuf,
    },

    /// List a directory for the file tree. Answered with [`EngineEvent::DirListing`].
    ListDir {
        path: std::path::PathBuf,
    },
    /// Load a file into an editor buffer. Answered with [`EngineEvent::FileOpened`].
    OpenFile {
        path: std::path::PathBuf,
    },
    /// Search the workspace. Answered with [`EngineEvent::SearchResults`].
    Search {
        query: crate::search::Query,
    },
    /// Ask for every file in the workspace, to complete an `@` mention against. Answered with
    /// [`EngineEvent::WorkspaceFiles`].
    RequestWorkspaceFiles,
    /// Rewrite every match in the workspace.
    ///
    /// Destructive and not individually approved — the user confirms the count once, in the UI,
    /// having seen the matches this replaces. Kept a separate command from [`Self::Search`] so a
    /// keystroke in the search box can never be one character away from rewriting the tree.
    Replace {
        query: crate::search::Query,
        /// Inserted verbatim. `$1` is three characters here, not a capture group.
        replacement: String,
    },
    /// Write an editor buffer back to disk.
    ///
    /// No approval gate: this is the user's own save, and the keystroke *is* the consent. The
    /// path is still checked, because the boundary is a property of the app rather than of who
    /// asked.
    SaveFile {
        path: std::path::PathBuf,
        content: String,
    },

    Shutdown,
}

/// Engine → UI. Broadcast, so several views can watch the same run.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    RunStarted {
        run: RunId,
        conv: ConvId,
    },
    /// A chunk of the model's response. The UI coalesces these before re-rendering.
    Delta {
        run: RunId,
        event: StreamEvent,
    },
    /// A tool wants to run and needs the user's explicit consent. The run is blocked until a
    /// matching [`EngineCommand::ResolveApproval`] arrives.
    ApprovalNeeded {
        run: RunId,
        call: ToolCallId,
        tool: String,
        /// Which agent is asking. `None` is the one the user is talking to; `Some(role)` is a
        /// sub-agent it delegated to. Shown on the modal, because consenting to `rm -rf` from a
        /// sub-agent the user never addressed is a different decision than consenting to one they
        /// asked for.
        agent: Option<String>,
        preview: crate::tool::ToolPreview,
    },
    ToolStarted {
        run: RunId,
        call: ToolCallId,
        tool: String,
        /// See [`Self::ApprovalNeeded::agent`].
        agent: Option<String>,
    },
    ToolFinished {
        run: RunId,
        call: ToolCallId,
        ok: bool,
        summary: String,
    },
    RunFinished {
        run: RunId,
        stop: StopReason,
        usage: Usage,
    },
    Error {
        run: Option<RunId>,
        message: String,
    },

    /// Auto-approve's state, as the *engine* has it. The UI shows this rather than what it last
    /// sent, so the badge on screen can never claim the prompts are back when they are not.
    AutoApprove(bool),
    /// The current provider list, sent on request and after every change.
    Providers {
        providers: Vec<ProviderSummary>,
        /// Whether an OS keychain is reachable. When false the settings UI says so up front
        /// rather than letting the user type a key that cannot be stored.
        keychain: bool,
    },
    /// The models a provider reports, in the order it returned them — which is usually
    /// newest-first, and is a better default ordering than anything we could impose.
    Models {
        provider: String,
        models: Vec<crate::provider::ModelInfo>,
    },
    /// The saved workspaces and the root currently in force.
    ///
    /// `active` is the *effective* root, so it is not always one of `saved` — with nothing saved
    /// it is the process working directory.
    Workspaces {
        saved: Vec<std::path::PathBuf>,
        active: std::path::PathBuf,
    },
    /// One directory's contents, already filtered to what can actually be opened.
    DirListing {
        path: std::path::PathBuf,
        entries: Vec<crate::files::Entry>,
        /// True when the directory was too large to send whole, so the tree can say so rather
        /// than quietly showing a prefix as if it were everything.
        truncated: bool,
    },
    /// A file's contents, for an editor tab.
    FileOpened {
        path: std::path::PathBuf,
        content: String,
    },
    /// A save landed on disk. The UI clears the tab's modified marker on this, never on send —
    /// marking it clean when the write might still fail is how unsaved work gets lost.
    FileSaved {
        path: std::path::PathBuf,
    },
    /// Workspace-relative paths for `@` completion, filtered by the same policy as everything
    /// else — a file that cannot be attached is never offered.
    WorkspaceFiles {
        files: Vec<std::path::PathBuf>,
    },
    /// Matches for the query the UI last sent.
    SearchResults {
        query: String,
        results: crate::search::Results,
    },
    /// A replace finished. The UI re-runs the search on this, because every hit it was showing
    /// now points at text that is no longer there.
    Replaced {
        files: usize,
        replacements: usize,
    },
    /// A log line, mirrored from `tracing` for the in-app viewer.
    Log(LogRecord),
}

/// A provider as the settings UI is allowed to see it.
///
/// Carries `has_key`, never the key. The secret is read in the engine at request time and has no
/// path back across the channel — not even to our own UI.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderSummary {
    pub entry: ProviderEntry,
    pub has_key: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub ts: String,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
        }
    }
}

/// Provider-neutral streaming events. Every provider codec normalises into this.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallStart { id: String, name: String },
    /// Providers stream tool arguments as JSON *fragments*, not whole values.
    ToolCallDelta { id: String, args_json: String },
    ToolCallEnd { id: String },
    Usage(Usage),
    Done(StopReason),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    Cancelled,
    Refusal,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderKind;

    /// `EngineCommand` derives `Debug` and carries an API key on its way to the keychain, so a
    /// single `tracing::debug!(?cmd)` anywhere in the dispatch loop would print it. `SecretString`
    /// is what stops that, and it is worth asserting rather than trusting.
    #[test]
    fn debug_on_a_command_never_prints_the_key() {
        let cmd = EngineCommand::SaveProvider {
            entry: ProviderEntry {
                name: "openai".into(),
                kind: ProviderKind::OpenAi,
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o-mini".into(),
                context_window: None,
                needs_key: true,
                org_id: None,
                headers: Default::default(),
            },
            key: Some(SecretString::from("sk-SUPERSECRET123")),
        };

        let rendered = format!("{cmd:?}");
        assert!(!rendered.contains("SUPERSECRET"), "key leaked into Debug: {rendered}");
        assert!(rendered.contains("openai"), "the rest must stay debuggable: {rendered}");
    }
}
