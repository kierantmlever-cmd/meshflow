//! Workspace search, and replace across it.
//!
//! Search is harmless and runs on demand. **Replace is not**: it rewrites every match in the
//! workspace in one go, with no per-file approval, and nothing undoes it. So it is deliberately
//! awkward — the button only appears once there are results to look at, it states the exact
//! counts, and it takes a second, differently-labelled click to go through. The user is
//! consenting to a number they have been shown, not to a verb.

use freya::prelude::*;
use mf_engine::{
    proto::EngineCommand,
    search::{Query, Results},
};

use crate::{Bridge, state::AppState, theme::Theme};

/// What the search panel is currently showing.
#[derive(Clone, Default, PartialEq)]
pub struct SearchState {
    /// The query the *results* belong to. Empty means the panel is showing the file tree.
    pub query: String,
    pub replacement: String,
    pub case_sensitive: bool,
    /// Read the query as a pattern rather than as the text it is.
    pub regex: bool,
    pub results: Results,
    /// True once Replace has been asked for and is waiting on the confirm click.
    pub confirming: bool,
    pub notice: String,
}

#[derive(PartialEq)]
pub struct SearchBox;

impl Component for SearchBox {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();

        // Local so typing does not re-render the results list on every keystroke; the shared
        // state only moves when a search actually runs.
        let typed = use_state(String::new);
        let replacement = use_state(String::new);

        let mut search = state.search;
        let snapshot = search.read().clone();

        let run = {
            let cmd_tx = bridge.cmd_tx.clone();
            move || {
                let query = typed.read().trim().to_owned();
                let mut w = search.write();
                w.confirming = false;
                w.notice = String::new();
                if query.is_empty() {
                    // Clearing the box is how you get back to the tree.
                    *w = SearchState::default();
                    return;
                }
                w.query = query.clone();
                let query =
                    Query { text: query, case_sensitive: w.case_sensitive, regex: w.regex };
                drop(w);
                let _ = cmd_tx.send(EngineCommand::Search { query });
            }
        };

        // Both switches re-run immediately: leaving stale results under a flipped switch would
        // show matches that the current settings do not produce — and Replace acts on the
        // settings, not on the rows.
        let toggle_case = {
            let mut run = run.clone();
            move |_| {
                let next = !search.read().case_sensitive;
                search.write().case_sensitive = next;
                if !search.read().query.is_empty() {
                    run();
                }
            }
        };

        let toggle_regex = {
            let mut run = run.clone();
            move |_| {
                let next = !search.read().regex;
                search.write().regex = next;
                if !search.read().query.is_empty() {
                    run();
                }
            }
        };

        let ask_replace = {
            let tabs = state.tabs;
            move |_| {
                let hits = search.read().results.hits.clone();
                if hits.is_empty() {
                    return;
                }

                // Refused rather than raced. A replace rewrites the file on disk while the editor
                // still holds an older buffer, so the next Ctrl+S would put the pre-replace text
                // straight back and quietly undo it.
                let unsaved: Vec<String> = tabs
                    .read()
                    .iter()
                    .filter(|t| t.data.read().is_edited() && hits.iter().any(|h| h.path == t.path))
                    .map(|t| t.name())
                    .collect();

                let mut w = search.write();
                if !unsaved.is_empty() {
                    w.notice = format!(
                        "Save or close {} first — replacing on disk would be undone by the next \
                         save from that tab.",
                        unsaved.join(", ")
                    );
                    return;
                }
                w.notice = String::new();
                w.confirming = true;
            }
        };

        let confirm_replace = {
            let cmd_tx = bridge.cmd_tx.clone();
            move |_| {
                let w = search.read();
                let query =
                    Query { text: w.query.clone(), case_sensitive: w.case_sensitive, regex: w.regex };
                drop(w);
                let _ = cmd_tx.send(EngineCommand::Replace {
                    query,
                    replacement: replacement.read().clone(),
                });
                search.write().confirming = false;
            }
        };

        let hits = snapshot.results.hits.len();
        let files = {
            let mut paths: Vec<&std::path::PathBuf> =
                snapshot.results.hits.iter().map(|h| &h.path).collect();
            paths.sort();
            paths.dedup();
            paths.len()
        };

        rect()
            .width(Size::fill())
            .direction(Direction::Vertical)
            .spacing(theme.gap(6.))
            .padding(theme.gap(8.))
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Center)
                    .spacing(theme.gap(6.))
                    .content(Content::flex())
                    .child(
                        Input::new(typed)
                            .width(Size::flex(1.))
                            .placeholder("Search files")
                            // Enter, not every keystroke: each search walks the whole workspace,
                            // so searching per character would spend the tree's worth of IO on
                            // prefixes nobody asked about.
                            .on_submit({
                                let mut run = run.clone();
                                move |_: String| run()
                            }),
                    )
                    .child(Button::new().on_press(move |_| run.clone()()).child("Find")),
            )
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Center)
                    .spacing(theme.gap(6.))
                    .child(
                        Switch::new().toggled(snapshot.case_sensitive).on_toggle(toggle_case),
                    )
                    .child(
                        label()
                            .color(theme.text_dim)
                            .font_size(theme.font_size - 3.)
                            .text("Match case"),
                    )
                    .child(Switch::new().toggled(snapshot.regex).on_toggle(toggle_regex))
                    .child(
                        label()
                            .color(theme.text_dim)
                            .font_size(theme.font_size - 3.)
                            .text("Regex"),
                    ),
            )
            .map((!snapshot.query.is_empty()).then_some(()), |root, ()| {
                root.child(
                    label().color(theme.text_dim).font_size(theme.font_size - 3.).text(
                        if hits == 0 {
                            format!("No matches in {} files", snapshot.results.files_searched)
                        } else if snapshot.results.truncated {
                            format!("First {hits} matches in {files} files")
                        } else {
                            format!("{hits} matches in {files} files")
                        },
                    ),
                )
            })
            // Only offered once there is something to replace, so the destructive control is
            // never sitting there next to an empty box.
            .map((hits > 0).then_some(()), |root, ()| {
                root.child(
                    rect()
                        .width(Size::fill())
                        .direction(Direction::Horizontal)
                        .cross_align(Alignment::Center)
                        .spacing(theme.gap(6.))
                        .content(Content::flex())
                        .child(
                            Input::new(replacement)
                                .width(Size::flex(1.))
                                // Says "literal" because in regex mode the obvious expectation is
                                // that `$1` means something. It does not — see `search::replace`.
                                .placeholder("Replace with (literal)"),
                        )
                        // "at least", because the hit list stops at 500 while the replace does
                        // not. A broad pattern like `\w+` hits that cap immediately, and a
                        // confirmation that understates what it rewrites is not consent.
                        .child(if snapshot.confirming {
                            // Different label, not the same button twice: "Replace" clicked twice
                            // by muscle memory must not be how a workspace gets rewritten.
                            Button::new()
                                .filled()
                                .on_press(confirm_replace)
                                .child(if snapshot.results.truncated {
                                    format!("Replace at least {hits}?")
                                } else {
                                    format!("Replace {hits}?")
                                })
                                .into_element()
                        } else {
                            Button::new().on_press(ask_replace).child("Replace all").into_element()
                        }),
                )
            })
            .map(snapshot.confirming.then_some(()), |root, ()| {
                root.child(
                    label().color(theme.danger).font_size(theme.font_size - 3.).text(
                        if snapshot.results.truncated {
                            format!(
                                "Rewrites every match in the workspace — at least {hits}, across \
                                 more than {files} files. This cannot be undone.",
                            )
                        } else {
                            format!(
                                "Rewrites {hits} matches across {files} files. This cannot be \
                                 undone.",
                            )
                        },
                    ),
                )
            })
            .map((!snapshot.notice.is_empty()).then_some(()), |root, ()| {
                root.child(
                    label()
                        .color(theme.danger)
                        .font_size(theme.font_size - 3.)
                        .text(snapshot.notice.clone()),
                )
            })
    }
}

/// Result rows, grouped under a heading per file.
pub fn result_rows(theme: &Theme, state: &AppState, bridge: &Bridge) -> Vec<Element> {
    let snapshot = state.search.read().clone();
    let mut rows = Vec::new();
    let mut current: Option<std::path::PathBuf> = None;

    for hit in &snapshot.results.hits {
        if current.as_ref() != Some(&hit.path) {
            let name = hit
                .path
                .strip_prefix(&*state.workspace.read())
                .unwrap_or(&hit.path)
                .display()
                .to_string();
            rows.push(
                label()
                    .color(theme.accent)
                    .font_size(theme.font_size - 2.)
                    .margin((theme.gap(8.), 0., theme.gap(2.), 0.))
                    .text(name)
                    .into_element(),
            );
            current = Some(hit.path.clone());
        }

        let open = {
            let (cmd_tx, path) = (bridge.cmd_tx.clone(), hit.path.clone());
            let (tabs, mut active_tab) = (state.tabs, state.active_tab);
            move |_| {
                if let Some(i) = tabs.read().iter().position(|t| t.path == path) {
                    active_tab.set(i);
                    return;
                }
                let _ = cmd_tx.send(EngineCommand::OpenFile { path: path.clone() });
            }
        };

        rows.push(
            rect()
                .width(Size::fill())
                .direction(Direction::Horizontal)
                .spacing(theme.gap(6.))
                .padding((theme.gap(2.), theme.gap(4.)))
                .corner_radius(4.)
                .on_press(open)
                .child(
                    label()
                        .color(theme.text_dim)
                        .font_family("JetBrains Mono")
                        .font_size(theme.font_size - 3.)
                        .text(hit.line.to_string()),
                )
                .child(
                    label()
                        .color(theme.text)
                        .font_family("JetBrains Mono")
                        .font_size(theme.font_size - 3.)
                        // Trimmed for display only. A hit deep in an indented block would
                        // otherwise render as a row of blank space.
                        .text(hit.text.trim().to_owned()),
                )
                .into_element(),
        );
    }
    rows
}
