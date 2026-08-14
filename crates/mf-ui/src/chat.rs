//! Streaming chat view.
//!
//! Pure rendering. The event drain and the transcript live in [`crate::state`], at the root, so
//! that a reply keeps streaming while the user is on another tab.

use freya::prelude::*;
use mf_engine::proto::{EngineCommand, RunId};

use crate::{Bridge, state::AppState, theme::Theme};

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
                let _ = bridge.cmd_tx.send(EngineCommand::SendUserMessage { conv, text });
                draft.set(String::new());
            }
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
            .child(composer(&theme, draft, submit, state.active_run, bridge))
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

fn composer(
    theme: &Theme,
    draft: State<String>,
    submit: impl FnMut(String) + 'static,
    active_run: State<Option<RunId>>,
    bridge: Bridge,
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
            Input::new(draft)
                .placeholder("Ask anything…")
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
