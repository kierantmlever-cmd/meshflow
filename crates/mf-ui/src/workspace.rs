//! The workspace picker.
//!
//! A workspace root is not a convenience setting — it is the boundary every agent's file access is
//! checked against. So this screen prints the active root **in full**, says plainly what it grants,
//! and calls removal "Forget" rather than "Delete" because it never touches the directory itself.

use std::path::{Path, PathBuf};

use freya::prelude::*;
use mf_engine::proto::EngineCommand;
use tokio::sync::mpsc;

use crate::{Bridge, state::AppState, theme::Theme};

/// Components kept when a path is too long for the header.
const TAIL: usize = 3;

/// Open the OS folder picker and make the chosen directory the workspace.
///
/// The dialog is the *system's*, not ours: it browses under the user's own privileges and hands
/// back a single path, so the engine never lists a directory outside the sandbox just to let
/// someone pick one. The engine still validates what comes back — a picker is a convenience, not
/// a trust boundary.
///
/// A cancelled dialog and an unreachable portal are indistinguishable here (both are `None`), so
/// the typed path field stays as the way through on a machine with no portal running.
pub fn choose_folder(cmd_tx: mpsc::UnboundedSender<EngineCommand>, start: PathBuf) {
    // Freya's `spawn`, not `tokio::spawn`: this touches nothing `Send`, and the portal call is a
    // D-Bus round trip that must not block the frame while the user is deciding.
    spawn(async move {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Choose a workspace folder")
            .set_directory(start);

        match dialog.pick_folder().await {
            Some(folder) => {
                let _ = cmd_tx.send(EngineCommand::SetWorkspace { root: folder.path().to_path_buf() });
            }
            None => tracing::debug!("folder picker dismissed"),
        }
    });
}

/// The button, wherever a workspace can be chosen.
pub fn choose_folder_button(
    cmd_tx: mpsc::UnboundedSender<EngineCommand>,
    start: PathBuf,
    label: &'static str,
) -> impl IntoElement + use<> {
    Button::new()
        .on_press(move |_| choose_folder(cmd_tx.clone(), start.clone()))
        .child(label)
}

/// A path as a person reads it: `~` for home, and only the tail when it is deep.
///
/// For the header, which has no room for a long path. The workspace screen prints the root in
/// full — an abbreviated boundary is not something to make a security decision on.
pub fn short_path(path: &Path) -> String {
    let home = std::env::home_dir();
    let text = match home.as_ref().and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };

    let parts: Vec<&str> = text.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= TAIL {
        return text;
    }
    format!("…/{}", parts[parts.len() - TAIL..].join("/"))
}

/// Workspace section of the settings screen.
#[derive(PartialEq)]
pub struct WorkspaceSection;

impl Component for WorkspaceSection {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();

        let mut typed = use_state(String::new);
        let mut notice = use_state(String::new);

        // The typed-path fallback. `choose_folder` above is the way in; this stays because a
        // cancelled dialog and a machine with no portal are the same `None`, and a headless box
        // still needs a way to set a workspace.
        let open = {
            let cmd_tx = bridge.cmd_tx.clone();
            move |_| {
                let root = typed.read().trim().to_owned();
                if root.is_empty() {
                    notice.set("Type a directory to open.".into());
                    return;
                }

                // Checked here so the answer to a typo appears next to the button that caused it.
                // The engine's own error lands in the shared error slot at the far end of this
                // screen, which for a mistyped path reads as nothing happening at all. This is
                // for the *message*; the engine still validates, because it is the trust boundary
                // and the directory can vanish between this click and the switch.
                let expanded = mf_engine::expand_home(Path::new(&root));
                if !expanded.is_dir() {
                    notice.set(format!("{} is not a directory that exists.", expanded.display()));
                    return;
                }

                let _ = cmd_tx.send(EngineCommand::SetWorkspace { root: PathBuf::from(root) });
                // Cleared only once the path is worth sending — a rejected one stays on screen
                // to be corrected rather than making the user retype it.
                typed.set(String::new());
                notice.set(String::new());
            }
        };

        let active = state.workspace.read().clone();
        let saved = state.workspaces.read().clone();

        rect()
            .width(Size::fill())
            .direction(Direction::Vertical)
            .spacing(theme.gap(10.))
            .child(heading(&theme, "Workspace"))
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Vertical)
                    .spacing(theme.gap(4.))
                    .padding(theme.gap(12.))
                    .corner_radius(8.)
                    .background(theme.surface)
                    // In full, never abbreviated: this is the sentence the user is trusting.
                    .child(label().color(theme.accent).text(active.display().to_string()))
                    .child(
                        label()
                            .color(theme.text_dim)
                            .font_size(theme.font_size - 3.)
                            .text(
                                "Agents can read and write anything under this directory, and \
                                 nothing outside it. Writes still need your approval. Credentials, \
                                 SSH keys, .env files and VCS metadata stay denied here too.",
                            ),
                    ),
            )
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Center)
                    .spacing(theme.gap(8.))
                    // Without this the flex below is inert and the button falls off the row.
                    .content(Content::flex())
                    // Browsing first, typing second: the picker is how anyone actually finds a
                    // folder, and the field is the fallback when no portal is running.
                    .child(
                        Button::new()
                            .filled()
                            .on_press({
                                let (cmd_tx, start) = (bridge.cmd_tx.clone(), active.clone());
                                move |_| choose_folder(cmd_tx.clone(), start.clone())
                            })
                            .child("Choose folder…"),
                    )
                    .child(
                        Input::new(typed).width(Size::flex(1.)).placeholder("or type ~/code/my-project"),
                    )
                    .child(Button::new().on_press(open).child("Open")),
            )
            .map((!notice.read().is_empty()).then_some(()), |root, ()| {
                root.child(label().color(theme.danger).text(notice.read().clone()))
            })
            .map((!saved.is_empty()).then_some(()), |root, ()| {
                root.child(
                    rect()
                        .width(Size::fill())
                        .direction(Direction::Vertical)
                        .spacing(theme.gap(6.))
                        .children(
                            saved
                                .iter()
                                .map(|w| {
                                    workspace_row(&theme, w, w == &active, &bridge).into_element()
                                })
                                .collect::<Vec<_>>(),
                        ),
                )
            })
    }
}

fn workspace_row(
    theme: &Theme,
    root: &Path,
    active: bool,
    bridge: &Bridge,
) -> impl IntoElement + use<> {
    let use_it = {
        let (cmd_tx, root) = (bridge.cmd_tx.clone(), root.to_path_buf());
        move |_| {
            let _ = cmd_tx.send(EngineCommand::SetWorkspace { root: root.clone() });
        }
    };
    let forget = {
        let (cmd_tx, root) = (bridge.cmd_tx.clone(), root.to_path_buf());
        move |_| {
            let _ = cmd_tx.send(EngineCommand::ForgetWorkspace { root: root.clone() });
        }
    };

    rect()
        .width(Size::fill())
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(theme.gap(10.))
        .padding(theme.gap(10.))
        .corner_radius(8.)
        .background(theme.surface)
        .content(Content::flex())
        .child(
            label()
                .width(Size::flex(1.))
                .color(if active { theme.accent } else { theme.text })
                .text(if active {
                    format!("{}  ·  in use", root.display())
                } else {
                    root.display().to_string()
                }),
        )
        .map((!active).then_some(()), |row, ()| {
            row.child(Button::new().on_press(use_it).child("Use"))
        })
        // "Forget", not "Delete": this removes the entry from a list. Nothing on disk changes,
        // and a button that reads as if it might is a button nobody dares press.
        .child(Button::new().on_press(forget).child("Forget"))
}

fn heading(theme: &Theme, text: &str) -> impl IntoElement + use<> {
    label().color(theme.text).font_size(theme.font_size + 3.).text(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_path_abbreviates_from_the_front() {
        // The tail is what identifies a project; the prefix is what everything shares.
        assert_eq!(short_path(Path::new("/a/b/c/d/e")), "…/c/d/e");
        assert_eq!(short_path(Path::new("/srv/proj")), "/srv/proj");
    }

    #[test]
    fn short_path_uses_a_tilde_for_home() {
        let Some(home) = std::env::home_dir() else { return };
        assert_eq!(short_path(&home), "~");
        assert_eq!(short_path(&home.join("code")), "~/code");
        // Still abbreviated once deep, and the `~` counts as a component so the result stays short.
        assert_eq!(short_path(&home.join("a/b/c/d")), "…/b/c/d");
    }
}
