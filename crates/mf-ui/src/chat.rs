//! Streaming chat view.
//!
//! Pure rendering. The event drain and the transcript live in [`crate::state`], at the root, so
//! that a reply keeps streaming while the user is on another tab.

use std::path::{Path, PathBuf};

use freya::prelude::*;
use mf_engine::proto::{EngineCommand, RunId};

use crate::{Bridge, state::AppState, theme::Theme};

/// Completions offered for an `@` mention. A list long enough to scroll is a list nobody reads —
/// the answer to "too many matches" is to type another character.
const MAX_COMPLETIONS: usize = 8;

/// How many sub-agents the composer will offer. Matches the engine's own ceiling, which clamps
/// whatever arrives regardless of what this UI sends.
const MAX_AGENTS: u8 = mf_engine::MAX_AGENTS;

/// How many separable jobs a request looks like it contains.
///
/// Counts the structure people actually use when they mean "these things": bullet lines, numbered
/// lines, and steps joined by "then". Prose is left alone at 1 — "read the config and tell me
/// what is wrong" is one job described in two clauses, and a heuristic that reads every "and" as
/// a task would double the bill on ordinary sentences.
///
/// ponytail: deliberately crude, because it only picks the *default* of a control the user can
/// see and change. Make it cleverer only if the number it suggests is routinely wrong.
fn suggested_agents(draft: &str) -> u8 {
    let listed = draft
        .lines()
        .filter(|line| {
            let line = line.trim_start();
            line.starts_with("- ")
                || line.starts_with("* ")
                || line
                    .split_once(['.', ')'])
                    .is_some_and(|(head, rest)| {
                        !head.is_empty()
                            && head.len() <= 2
                            && head.bytes().all(|b| b.is_ascii_digit())
                            && rest.starts_with(' ')
                    })
        })
        .count();

    let steps = if listed >= 2 {
        listed
    } else {
        // "do X then Y then Z" — three jobs, two separators.
        draft.to_lowercase().matches(" then ").count() + 1
    };

    steps.clamp(1, MAX_AGENTS as usize) as u8
}

/// The `@` fragment the composer is completing, if the draft ends in one.
///
/// Only ever the *last* word: without a caret position there is no way to know which mention an
/// edit in the middle of the line belongs to, and completing the wrong one would rewrite text the
/// user had finished with. The `@` must open a word, matching how the engine reads mentions —
/// `user@host` is an address in both places.
fn mention_fragment(draft: &str) -> Option<&str> {
    let at = draft.rfind('@')?;
    if at > 0 && !draft.as_bytes()[at - 1].is_ascii_whitespace() {
        return None;
    }
    let fragment = &draft[at + 1..];
    (!fragment.contains(char::is_whitespace)).then_some(fragment)
}

/// Workspace files matching `fragment`, best first.
///
/// Matched anywhere in the path, so `agent` finds `crates/mf-engine/src/agent.rs` without the
/// user retyping a directory they can already see. A hit on the file *name* outranks one that is
/// only in the directory part — typing `search` means `search.rs`, not everything under `src/`.
fn completions<'a>(files: &'a [PathBuf], fragment: &str) -> Vec<&'a Path> {
    let needle = fragment.to_lowercase();
    let mut matched: Vec<&Path> = files
        .iter()
        .map(PathBuf::as_path)
        .filter(|path| path.to_string_lossy().to_lowercase().contains(&needle))
        .collect();

    matched.sort_by_key(|path| {
        let name_hit = path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().to_lowercase().contains(&needle));
        (!name_hit, path.as_os_str().len())
    });
    matched.truncate(MAX_COMPLETIONS);
    matched
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Author {
    User,
    Assistant,
    /// A tool invocation and its outcome, shown inline so the run is auditable.
    Tool,
    Error,
}

#[derive(Clone, PartialEq)]
pub struct ChatMessage {
    pub author: Author,
    pub text: String,
    /// While true the bubble shows a caret and renders as plain text — half-parsed markdown
    /// flickers badly (an unclosed fence would style the rest of the message as code).
    pub streaming: bool,
}

#[derive(PartialEq)]
pub struct ChatView;

impl Component for ChatView {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();
        let draft = use_state(String::new);
        // `None` while the count follows the draft. Any use of the stepper pins it, because a
        // number that silently moved after the user set it would be the app overruling them.
        let pinned = use_state(|| None::<u8>);

        let agents = match *pinned.read() {
            Some(pinned) => pinned,
            None => suggested_agents(&draft.read()),
        };

        let submit = {
            let bridge = bridge.clone();
            let (mut messages, active_run, conv) = (state.messages, state.active_run, state.conv);
            let mut draft = draft;
            move |text: String| {
                let text = text.trim().to_owned();
                if text.is_empty() || active_run.read().is_some() {
                    return;
                }
                messages.write().push(ChatMessage {
                    author: Author::User,
                    text: text.clone(),
                    streaming: false,
                });
                let _ = bridge.cmd_tx.send(EngineCommand::SendUserMessage {
                    conv,
                    text,
                    max_agents: agents,
                });
                draft.set(String::new());
            }
        };

        // Refreshed when a mention *starts*, not per keystroke: one walk per `@` the user types,
        // and the list is current at the moment it is used rather than as of app launch.
        use_side_effect({
            let cmd_tx = bridge.cmd_tx.clone();
            move || {
                if mention_fragment(&draft.read()) == Some("") {
                    let _ = cmd_tx.send(EngineCommand::RequestWorkspaceFiles);
                }
            }
        });

        let suggestions: Vec<Element> = match mention_fragment(&draft.read()) {
            Some(fragment) => {
                let files = state.workspace_files.read();
                completions(&files, fragment)
                    .into_iter()
                    .map(|path| {
                        let shown = path.display().to_string();
                        let (mut draft, path) = (draft, path.to_path_buf());
                        let insert = move |_| {
                            // The read guard is scoped and dropped before the write: holding one
                            // across `set` panics the app outright — Freya's state is a RefCell,
                            // and a borrow still live when the write lands is not a warning.
                            let completed = {
                                let text = draft.read();
                                // Rewrites from the `@` to the end, so the fragment already
                                // typed is replaced rather than appended to.
                                let head = &text[..text.rfind('@').unwrap_or(text.len())];
                                format!("{head}@{} ", path.display())
                            };
                            draft.set(completed);
                        };
                        rect()
                            .width(Size::fill())
                            .padding((theme.gap(3.), theme.gap(8.)))
                            .corner_radius(4.)
                            .on_press(insert)
                            .child(
                                label()
                                    .color(theme.text)
                                    .font_family("JetBrains Mono")
                                    .font_size(theme.font_size - 3.)
                                    .text(shown),
                            )
                            .into_element()
                    })
                    .collect()
            }
            None => Vec::new(),
        };

        let bubbles: Vec<Element> = state
            .messages
            .read()
            .iter()
            .map(|m| bubble(m, &theme).into_element())
            .collect();

        rect()
            .expanded()
            .background(theme.bg)
            .direction(Direction::Vertical)
            // Size::Flex is inert unless the parent opts into flex content — without this the
            // ScrollView eats the column and the composer is pushed off-screen.
            .content(Content::flex())
            .child(
                ScrollView::new()
                    .direction(Direction::Vertical)
                    .spacing(theme.gap(12.))
                    .height(Size::flex(1.))
                    .child(
                        rect()
                            .width(Size::fill())
                            .direction(Direction::Vertical)
                            .spacing(theme.gap(12.))
                            .padding((theme.gap(16.), theme.gap(20.)))
                            .children(bubbles),
                    ),
            )
            // Above the composer, so the list grows towards the transcript instead of pushing the
            // input the user is typing into off the bottom of the window.
            //
            // Always in the tree, empty or not. Adding the row conditionally shifts the
            // composer's index among its siblings, which remounts the `Input` and resets its
            // caret to 0 — typing `@` and then a letter put the letter at the *start* of the
            // line. An empty rect with no padding takes no space, so the cost is one node.
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Vertical)
                    .padding(if suggestions.is_empty() {
                        (0., 0.)
                    } else {
                        (theme.gap(6.), theme.gap(20.))
                    })
                    .background(if suggestions.is_empty() { theme.bg } else { theme.surface_alt })
                    .children(suggestions),
            )
            .child(composer(&theme, draft, submit, state.active_run, bridge, agents, pinned))
    }
}

fn bubble(msg: &ChatMessage, theme: &Theme) -> impl IntoElement {
    let (background, accent, author) = match msg.author {
        Author::User => (theme.surface_alt, theme.accent, "You"),
        Author::Assistant => (theme.surface, theme.text_dim, "Assistant"),
        Author::Tool => (theme.bg, theme.accent, "Tool"),
        Author::Error => (theme.surface, theme.danger, "Error"),
    };

    let body = match msg.author {
        // Plain text while streaming: partially-parsed markdown flickers, and an unclosed fence
        // would restyle everything after it on every flush.
        Author::Assistant if !msg.streaming => MarkdownViewer::new(msg.text.clone()).into_element(),
        _ => label()
            .color(if msg.author == Author::Error { theme.danger } else { theme.text })
            .text(if msg.streaming && msg.text.is_empty() {
                "…".to_owned()
            } else {
                msg.text.clone()
            })
            .into_element(),
    };

    // Deliberately a plain vertical stack. A full-height accent rule alongside the text needs
    // `Size::fill()`, which resolves against the scroll viewport rather than the row — one
    // message then swallows the whole visible area.
    rect()
        .width(Size::fill())
        .direction(Direction::Vertical)
        .spacing(theme.gap(6.))
        .padding(theme.gap(12.))
        .corner_radius(10.)
        .background(background)
        .child(label().text(author).color(accent).font_size(theme.font_size - 3.))
        .child(body)
}

/// The `− n +` stepper that caps how many sub-agents a task may use.
///
/// Shows the count it would send and where that number came from. "auto" means it is following
/// the request as it is typed; pressing the number returns it to that after a manual change,
/// which is the only way back — otherwise pinning a number once pins it forever without saying so.
fn agent_stepper(
    theme: &Theme,
    agents: u8,
    mut pinned: State<Option<u8>>,
) -> impl IntoElement {
    let following = pinned.read().is_none();

    rect()
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(theme.gap(4.))
        .child(
            Button::new()
                .on_press(move |_| pinned.set(Some(agents.saturating_sub(1))))
                .child("−"),
        )
        .child(
            rect()
                // Pressing the count clears the pin, so the number goes back to following the
                // request. A fixed width stops the row twitching as the label changes.
                .width(Size::px(64.))
                .main_align(Alignment::Center)
                .on_press(move |_| pinned.set(None))
                .child(
                    label()
                        .color(if agents == 0 { theme.text_dim } else { theme.accent })
                        .font_size(theme.font_size - 2.)
                        .text(match (agents, following) {
                            // Not "0": what the user needs to know is that nothing will be
                            // delegated, and a zero reads as a count that happens to be empty.
                            (0, true) => "off · auto".to_owned(),
                            (0, false) => "off".to_owned(),
                            (n, true) => format!("{n} · auto"),
                            (n, false) => n.to_string(),
                        }),
                ),
        )
        .child(
            Button::new()
                .on_press(move |_| pinned.set(Some((agents + 1).min(MAX_AGENTS))))
                .child("+"),
        )
}

fn composer(
    theme: &Theme,
    draft: State<String>,
    submit: impl FnMut(String) + 'static,
    active_run: State<Option<RunId>>,
    bridge: Bridge,
    agents: u8,
    pinned: State<Option<u8>>,
) -> impl IntoElement {
    let busy = active_run.read().is_some();

    rect()
        .width(Size::fill())
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(theme.gap(10.))
        .padding((theme.gap(12.), theme.gap(20.)))
        .background(theme.surface)
        .content(Content::flex())
        .child(
            label()
                .color(theme.text_dim)
                .font_size(theme.font_size - 2.)
                .text("Agents"),
        )
        .child(agent_stepper(theme, agents, pinned))
        .child(
            Input::new(draft)
                // The `@` hint is the only discovery route there is — attachments are resolved in
                // the engine and nothing in the UI hints at them otherwise.
                .placeholder("Ask anything…  @path attaches a file")
                .width(Size::flex(1.))
                .auto_focus(true)
                .on_submit(submit),
        )
        .child(if busy {
            Button::new()
                .on_press(move |_| {
                    if let Some(run) = *active_run.read() {
                        let _ = bridge.cmd_tx.send(EngineCommand::CancelRun(run));
                    }
                })
                .child("Stop")
                .into_element()
        } else {
            label().color(theme.text_dim).text("⏎").into_element()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_only_the_mention_being_typed() {
        assert_eq!(mention_fragment("look at @src/ag"), Some("src/ag"));
        assert_eq!(mention_fragment("look at @"), Some(""));
        // Finished mentions are left alone — a space ends the fragment.
        assert_eq!(mention_fragment("@src/a.rs explain this"), None);
        // Only the last one is in play.
        assert_eq!(mention_fragment("@a.rs and @b"), Some("b"));
        // An address is not a mention, in the composer or in the engine.
        assert_eq!(mention_fragment("mail user@host"), None);
        assert_eq!(mention_fragment("nothing here"), None);
    }

    #[test]
    fn ordinary_prose_asks_for_one_agent() {
        // The expensive mistake: reading every "and" as a separate job, so a one-line question
        // fans out into a handful of paid model runs.
        assert_eq!(suggested_agents("read the config and tell me what is wrong"), 1);
        assert_eq!(suggested_agents(""), 1);
        assert_eq!(suggested_agents("port the auth module"), 1);
    }

    #[test]
    fn a_list_of_jobs_suggests_one_agent_each() {
        assert_eq!(
            suggested_agents("port these:\n- the auth module\n- the store\n- the CLI"),
            3,
        );
        assert_eq!(suggested_agents("1. audit deps\n2. update them\n3. run the tests"), 3);
        // A single bullet is a formatting choice, not a plan.
        assert_eq!(suggested_agents("do this:\n- one thing"), 1);
    }

    #[test]
    fn steps_joined_by_then_count_as_separate_jobs() {
        assert_eq!(suggested_agents("audit the deps then update them then run the tests"), 3);
        // "Then" inside a word is not a separator.
        assert_eq!(suggested_agents("strengthen the parser"), 1);
    }

    #[test]
    fn the_suggestion_never_exceeds_what_the_engine_allows() {
        let many = (0..40).map(|i| format!("- job {i}\n")).collect::<String>();
        assert_eq!(suggested_agents(&many), MAX_AGENTS);
    }

    #[test]
    fn ranks_a_filename_hit_above_a_directory_one() {
        let files: Vec<PathBuf> = [
            "crates/mf-engine/src/agent.rs",
            "agent/notes.md",
            "crates/mf-ui/src/chat.rs",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();

        let found = completions(&files, "agent");
        // `agent.rs` first: typing a name means the file, not the folder that shares the word.
        assert_eq!(found[0], Path::new("crates/mf-engine/src/agent.rs"));
        assert_eq!(found.len(), 2, "chat.rs does not match: {found:?}");
    }

    #[test]
    fn an_empty_fragment_offers_the_shortest_paths_first() {
        // Every file matches the moment `@` is typed, so the ordering is all the user has.
        let files: Vec<PathBuf> =
            ["deeply/nested/thing.rs", "a.rs"].iter().map(PathBuf::from).collect();
        assert_eq!(completions(&files, "")[0], Path::new("a.rs"));
    }
}
