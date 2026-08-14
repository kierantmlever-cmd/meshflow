//! Log viewer.
//!
//! Newest first, which is why there is no auto-scroll: the line you want is always the one at the
//! top. Scroll-to-bottom is a mechanism that exists to compensate for the opposite choice.
//!
//! This shows the live `tracing` stream. The durable security record is the `audit_log` table in
//! the database, which outlives the process — a tool call that ran is a row there whether or not
//! anyone had this tab open.

use freya::prelude::*;
use mf_engine::proto::LogLevel;

use crate::{state::AppState, theme::Theme};

/// Level filter. `Warnings` includes errors, because a filter that excluded them would hide the
/// thing you opened the tab to find.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Warnings,
    Errors,
}

impl Filter {
    const ALL: [Filter; 3] = [Filter::All, Filter::Warnings, Filter::Errors];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Warnings => "Warnings",
            Self::Errors => "Errors",
        }
    }

    fn admits(self, level: LogLevel) -> bool {
        match self {
            Self::All => true,
            Self::Warnings => level <= LogLevel::Warn,
            Self::Errors => level == LogLevel::Error,
        }
    }
}

#[derive(PartialEq)]
pub struct LogView;

impl Component for LogView {
    fn render(&self) -> impl IntoElement {
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();
        let mut filter = use_state(|| Filter::All);
        let current = *filter.read();

        let lines: Vec<Element> = state
            .logs
            .read()
            .iter()
            .filter(|r| current.admits(r.level))
            .map(|r| {
                let colour = match r.level {
                    LogLevel::Error => theme.danger,
                    LogLevel::Warn => theme.accent,
                    LogLevel::Info => theme.text,
                };
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .spacing(theme.gap(10.))
                    .padding((theme.gap(3.), theme.gap(4.)))
                    .child(
                        label()
                            .color(theme.text_dim)
                            .font_family("JetBrains Mono")
                            .font_size(theme.font_size - 2.)
                            .text(r.ts.clone()),
                    )
                    .child(
                        rect().width(Size::px(46.)).child(
                            label()
                                .color(colour)
                                .font_family("JetBrains Mono")
                                .font_size(theme.font_size - 2.)
                                .text(r.level.label()),
                        ),
                    )
                    .child(
                        label()
                            .color(theme.text_dim)
                            .font_family("JetBrains Mono")
                            .font_size(theme.font_size - 2.)
                            .text(r.target.clone()),
                    )
                    .child(
                        label()
                            .color(colour)
                            .font_family("JetBrains Mono")
                            .font_size(theme.font_size - 2.)
                            .text(r.message.clone()),
                    )
                    .into_element()
            })
            .collect();

        let empty = lines.is_empty();

        rect()
            .expanded()
            .direction(Direction::Vertical)
            .background(theme.bg)
            .content(Content::flex())
            .child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Horizontal)
                    .cross_align(Alignment::Center)
                    .spacing(theme.gap(12.))
                    .padding((theme.gap(8.), theme.gap(16.)))
                    .background(theme.surface)
                    .child(SegmentedButton::new().children(Filter::ALL.map(|f| {
                        ButtonSegment::new()
                            .selected(f == current)
                            .on_press(move |_| filter.set(f))
                            .child(f.label())
                            .into_element()
                    })))
                    .child(
                        // The path, not a button: exporting a log is copying this file, and a
                        // save dialog that writes a second copy is a worse answer than the path.
                        label()
                            .color(theme.text_dim)
                            .font_size(theme.font_size - 3.)
                            .text(match mf_engine::logging::log_dir() {
                                Some(dir) => format!("Full logs: {}", dir.display()),
                                None => "No log directory available on this platform".into(),
                            }),
                    ),
            )
            .child(
                ScrollView::new().direction(Direction::Vertical).height(Size::flex(1.)).child(
                    rect()
                        .width(Size::fill())
                        .direction(Direction::Vertical)
                        .padding((theme.gap(8.), theme.gap(16.)))
                        .child(if empty {
                            label()
                                .color(theme.text_dim)
                                .text("Nothing logged at this level yet.")
                                .into_element()
                        } else {
                            rect()
                                .width(Size::fill())
                                .direction(Direction::Vertical)
                                .children(lines)
                                .into_element()
                        }),
                ),
            )
    }
}
