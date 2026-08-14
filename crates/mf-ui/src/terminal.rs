//! Built-in terminal: a real PTY, via `freya-terminal`.
//!
//! The handle lives in [`AppState`], not here. This panel unmounts every time the user switches
//! tabs, and `TerminalHandle` closes the PTY when the last clone drops — so a handle owned by the
//! panel would kill the user's shell, and everything running in it, on a click.
//!
//! This is a plain terminal for the user, not a tool surface for agents. Agent commands go
//! through `run_command`, which is gated by the tool dispatcher and needs approval; nothing here
//! is reachable from a model.

use freya::{prelude::*, terminal::*};

use crate::{state::AppState, theme::Theme};

/// The user's own shell, started in `cwd`, falling back per platform.
fn user_shell(cwd: &std::path::Path) -> CommandBuilder {
    let program = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        if cfg!(windows) { "powershell.exe".to_owned() } else { "/bin/sh".to_owned() }
    });

    let mut cmd = CommandBuilder::new(program);
    // Without this the shell assumes a dumb terminal and drops colour and cursor addressing.
    cmd.env("TERM", "xterm-256color");
    cmd.cwd(cwd);
    cmd
}

#[derive(PartialEq)]
pub struct TerminalPanel;

impl Component for TerminalPanel {
    fn render(&self) -> impl IntoElement {
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();
        let a11y_id = use_a11y();

        // Spawned on first open rather than at startup: most sessions never open this tab, and a
        // shell process per launch is a cost nobody asked for.
        use_hook({
            let mut terminal = state.terminal;
            // The workspace as it stands when the shell starts. Switching workspaces later does
            // not move a running shell: the user may have a job in flight, and `cd` is theirs to
            // type. This is a plain terminal, not a sandbox — the boundary in the header describes
            // what *agents* may touch, not what the user may.
            let cwd = state.workspace.read().clone();
            move || {
                if terminal.read().is_none() {
                    match TerminalHandle::new(TerminalId::new(), user_shell(&cwd), None) {
                        Ok(handle) => terminal.set(Some(handle)),
                        Err(e) => tracing::error!(%e, "could not start a terminal"),
                    }
                }
                // Switching to this tab is an unambiguous request to type into it.
                a11y_id.request_focus();
            }
        });

        let handle = state.terminal.read().clone();

        rect().expanded().background(theme.bg).padding(theme.gap(8.)).child(match handle {
            Some(handle) => {
                let for_keys = handle.clone();
                Terminal::new(handle)
                    .a11y_id(a11y_id)
                    .font_family("JetBrains Mono")
                    .font_size(theme.font_size)
                    .background(theme.bg)
                    .foreground(theme.text)
                    .selection_color(theme.accent)
                    .on_mouse_down(move |_| a11y_id.request_focus())
                    .on_key_down(move |e: Event<KeyboardEventData>| {
                        let _ = for_keys.write_key(&e.key, e.modifiers);
                    })
                    .into_element()
            }
            // The reason is in the log, and the log is one tab away.
            None => label()
                .color(theme.danger)
                .text("Could not start a shell. See the Logs tab for the reason.")
                .into_element(),
        })
    }
}
