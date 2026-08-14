//! MeshFlow UI.
//!
//! Freya's reactivity is single-threaded and `!Send`, so nothing here may cross into a
//! `tokio::spawn`. The engine runs on the Tokio pool; this crate only sends [`EngineCommand`]s and
//! drains [`EngineEvent`]s using Freya's own `spawn`, which stays on the UI thread.

use freya::prelude::*;
use mf_engine::proto::{EngineCommand, EngineEvent};
use tokio::sync::{broadcast, mpsc};

pub mod approval;
pub mod chat;
pub mod files;
pub mod logs;
pub mod search;
pub mod skript;
pub mod settings;
pub mod shell;
pub mod state;
pub mod terminal;
pub mod theme;
pub mod workspace;

use theme::Theme;

/// Handles to the engine, provided at the root and consumed by any view that needs them.
///
/// Held as a `Sender` rather than a `Receiver` because [`App::render`] takes `&self` and may run
/// many times — each view subscribes for itself instead of sharing one consumed receiver.
#[derive(Clone)]
pub struct Bridge {
    pub cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    pub evt_tx: broadcast::Sender<EngineEvent>,
}

/// Two bridges are the same bridge when they address the same engine. Components hold this as a
/// prop, and Freya diffs props by equality — comparing by channel identity keeps a clone of the
/// handles from being read as a change.
impl PartialEq for Bridge {
    fn eq(&self, other: &Self) -> bool {
        self.cmd_tx.same_channel(&other.cmd_tx)
    }
}

pub fn launch_app(
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    evt_tx: broadcast::Sender<EngineEvent>,
) {
    launch(
        LaunchConfig::new()
            .with_default_font("Inter")
            .with_fallback_font("Noto Sans")
            .with_window(
                WindowConfig::new_app(MeshFlow { bridge: Bridge { cmd_tx, evt_tx } })
                    .with_title("MeshFlow")
                    // Wayland app_id / X11 class. Without it the window is unidentifiable to the
                    // compositor, which breaks window rules, taskbar grouping and icon matching.
                    .with_app_id("meshflow")
                    .with_size(1280., 800.)
                    .with_background(Theme::default().bg),
            ),
    )
}

pub struct MeshFlow {
    bridge: Bridge,
}

impl App for MeshFlow {
    fn render(&self) -> impl IntoElement {
        Root(self.bridge.clone())
    }
}

/// Everything below here is a real component scope.
///
/// Context has to be provided from one: Freya's widgets resolve their palette through
/// `try_consume_context::<State<Theme>>()` and silently fall back to the *light* theme when the
/// lookup misses, so providing it from `App::render` leaves half the UI light.
#[derive(PartialEq)]
struct Root(Bridge);

impl Component for Root {
    fn render(&self) -> impl IntoElement {
        let bridge = self.0.clone();
        use_provide_context(move || bridge);
        let theme = use_provide_context(Theme::default);
        // Everything that has to outlive a tab switch — the chat transcript, the log tail, the
        // terminal's PTY — plus the single engine-event drain.
        let state = state::use_app_state(&self.0);
        use_provide_context(move || state);
        // Provided at the *root* scope, not this one. Freya widgets resolve their palette by
        // walking up from their own scope and fall back to the light theme the moment the walk
        // misses; anchoring at the root means the walk always terminates on our theme.
        use_hook({
            let theme = theme.clone();
            move || {
                let state = State::create(theme.to_freya());
                provide_root_context(state);
                state
            }
        });

        rect()
            .expanded()
            .background(theme.bg)
            .color(theme.text)
            .font_size(theme.font_size)
            .child(shell::Shell)
    }
}
