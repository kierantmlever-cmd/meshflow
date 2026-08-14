//! The window frame: header, tab switching, and the approval modal.
//!
//! The modal lives here rather than in the chat view on purpose. A parked run is blocking on an
//! answer no matter which tab is open, so hiding the prompt behind a tab would leave the user
//! staring at a terminal wondering why their agent stopped.

use freya::prelude::*;

use crate::{
    approval::ApprovalModal, chat::ChatView, files::FilesPanel, logs::LogView,
    settings::SettingsView, state::AppState, terminal::TerminalPanel, theme::Theme,
    workspace::short_path,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Chat,
    Files,
    Terminal,
    Logs,
    Settings,
}

impl Panel {
    const ALL: [Panel; 5] =
        [Panel::Chat, Panel::Files, Panel::Terminal, Panel::Logs, Panel::Settings];

    fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Files => "Files",
            Self::Terminal => "Terminal",
            Self::Logs => "Logs",
            Self::Settings => "Settings",
        }
    }
}

#[derive(PartialEq)]
pub struct Shell;

impl Component for Shell {
    fn render(&self) -> impl IntoElement {
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();
        let mut panel = use_state(|| Panel::Chat);
        let current = *panel.read();

        let body = match current {
            Panel::Chat => ChatView.into_element(),
            Panel::Files => FilesPanel.into_element(),
            Panel::Terminal => TerminalPanel.into_element(),
            Panel::Logs => LogView.into_element(),
            Panel::Settings => SettingsView.into_element(),
        };

        rect()
            .expanded()
            .background(theme.bg)
            .direction(Direction::Vertical)
            // Size::Flex is inert unless the parent opts into flex content — without this the
            // body eats the column and the header is pushed off-screen.
            .content(Content::flex())
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Center)
                    .spacing(theme.gap(12.))
                    .padding((theme.gap(10.), theme.gap(16.)))
                    .background(theme.surface)
                    // Without this the spacer's `flex(1.)` below is inert and the workspace label
                    // sits jammed against the tabs instead of at the far end.
                    .content(Content::flex())
                    .child(
                        // Teal dot: solid when idle, dim while a run is in flight.
                        rect()
                            .width(Size::px(8.))
                            .height(Size::px(8.))
                            .corner_radius(4.)
                            .background(if state.active_run.read().is_some() {
                                theme.text_dim
                            } else {
                                theme.accent
                            }),
                    )
                    .child(label().color(theme.text).text("MeshFlow"))
                    .child(
                        SegmentedButton::new().children(Panel::ALL.map(|p| {
                            ButtonSegment::new()
                                .selected(p == current)
                                .on_press(move |_| panel.set(p))
                                .child(p.label())
                                .into_element()
                        })),
                    )
                    // Pushes the workspace to the far end. Sitting it *before* the tabs made
                    // their positions depend on the length of the path, so every workspace switch
                    // moved the tabs out from under the pointer.
                    .child(rect().width(Size::flex(1.)))
                    // On every tab while it is armed. A mode that silences every consent prompt
                    // must never be something you have to open Settings to discover is on.
                    .map(state.auto_approve.read().then_some(()), |root, ()| {
                        root.child(
                            rect()
                                .padding((theme.gap(3.), theme.gap(8.)))
                                .corner_radius(6.)
                                .background(theme.danger)
                                .child(
                                    label()
                                        .color(theme.bg)
                                        .font_size(theme.font_size - 3.)
                                        .text("AUTO-APPROVE"),
                                ),
                        )
                    })
                    // The sandbox boundary, on every tab. What an agent is allowed to touch is
                    // not something the user should have to open a settings screen to find out.
                    .child(
                        label()
                            .color(theme.text_dim)
                            .font_size(theme.font_size - 2.)
                            .text(short_path(&state.workspace.read())),
                    ),
            )
            .child(rect().width(Size::fill()).height(Size::flex(1.)).child(body))
            // One at a time: the engine parks the run on each call in turn, so a queue of stacked
            // modals would misrepresent what is actually blocked.
            .map(state.approvals.read().first().cloned(), |root, pending| {
                root.child(ApprovalModal { pending })
            })
    }
}
