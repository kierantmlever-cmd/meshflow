//! The approval modal.
//!
//! This is the consent surface for every destructive action, so it renders the operation
//! **verbatim** — the exact command line, the exact file contents. A prompt that summarises what
//! it is asking about is not consent, because the user cannot see what they are agreeing to.
//!
//! There is deliberately no default button and no dismiss-by-clicking-away: closing the modal
//! any other way would have to mean *something*, and neither silent allow nor silent deny is
//! honest. The user picks.

use freya::prelude::*;
use mf_engine::{
    proto::{EngineCommand, ToolCallId},
    tool::{Approval, PreviewKind, ToolPreview},
};

use crate::{Bridge, theme::Theme};

/// A tool call waiting on the user.
#[derive(Clone, PartialEq)]
pub struct PendingApproval {
    pub call: ToolCallId,
    pub tool: String,
    pub preview: ToolPreview,
}

/// Render a unified diff, one row per line, coloured by its prefix.
///
/// Colour is a second channel, not the only one: the `+`/`-` prefixes are kept in the text so the
/// diff stays readable when copied out, and so a colour-blind reader is not being asked to
/// approve a destructive change on a hue distinction alone.
fn diff_body(text: &str, theme: &Theme) -> Element {
    let rows: Vec<Element> = text
        .lines()
        .map(|line| {
            let (colour, background) = match line.as_bytes().first() {
                Some(b'+') => (theme.added, theme.added_bg),
                Some(b'-') => (theme.removed, theme.removed_bg),
                // Hunk separators read as structure, not content.
                Some(0xE2) => (theme.text_dim, theme.bg),
                _ => (theme.text_dim, theme.bg),
            };
            rect()
                .width(Size::fill())
                .background(background)
                .child(
                    label()
                        .text(line.to_owned())
                        .color(colour)
                        .font_family("JetBrains Mono")
                        .font_size(theme.font_size - 1.),
                )
                .into_element()
        })
        .collect();

    rect().width(Size::fill()).direction(Direction::Vertical).children(rows).into_element()
}

#[derive(PartialEq)]
pub struct ApprovalModal {
    pub pending: PendingApproval,
}

impl Component for ApprovalModal {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let pending = self.pending.clone();

        let answer = move |decision: Approval| {
            let bridge = bridge.clone();
            let call = pending.call;
            move |_| {
                let _ = bridge.cmd_tx.send(EngineCommand::ResolveApproval { call, decision });
            }
        };

        Popup::new()
            .child(PopupTitle::new(self.pending.preview.title.clone()))
            .child(
                PopupContent::new().child(
                    rect()
                        .width(Size::px(560.))
                        .direction(Direction::Vertical)
                        .spacing(theme.gap(10.))
                        .child(
                            label()
                                .text(match self.pending.preview.kind {
                                    // "wants to run" reads like a command; what follows is a
                                    // change to a file the user already has.
                                    PreviewKind::Diff => {
                                        format!("{} wants to apply this change:", self.pending.tool)
                                    }
                                    PreviewKind::Text => {
                                        format!("{} wants to run:", self.pending.tool)
                                    }
                                })
                                .color(theme.text_dim)
                                .font_size(theme.font_size - 1.),
                        )
                        .child(
                            // The operation itself, unedited and unsummarised.
                            rect()
                                .width(Size::fill())
                                .height(Size::px(340.))
                                .padding(theme.gap(12.))
                                .corner_radius(8.)
                                .background(theme.bg)
                                .child(ScrollView::new().child(match self.pending.preview.kind {
                                    PreviewKind::Diff => diff_body(&self.pending.preview.detail, &theme),
                                    PreviewKind::Text => label()
                                        .text(self.pending.preview.detail.clone())
                                        .color(theme.text)
                                        .font_family("JetBrains Mono")
                                        .font_size(theme.font_size - 1.)
                                        .into_element(),
                                })),
                        ),
                ),
            )
            .child(
                PopupButtons::new().child(
                    rect()
                        .direction(Direction::Horizontal)
                        .spacing(theme.gap(8.))
                        .cross_align(Alignment::Center)
                        .child(Button::new().on_press(answer(Approval::Deny)).child("Deny"))
                        .child(
                            Button::new()
                                .on_press(answer(Approval::AllowAlways))
                                .child("Always allow this tool"),
                        )
                        .child(
                            Button::new().filled().on_press(answer(Approval::Allow)).child("Allow"),
                        ),
                ),
            )
    }
}
