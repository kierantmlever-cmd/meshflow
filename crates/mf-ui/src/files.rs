//! File tree and editor tabs.
//!
//! Nothing here touches the filesystem directly. Every listing, read and write goes to the engine
//! and comes back as an event — see `mf_engine::files` for why that matters, and because a
//! directory on a cold disk read from this thread would freeze the frame.
//!
//! Buffers are created in [`ScopeId::ROOT`], not in this panel's scope. This panel unmounts every
//! time the user switches tabs, and a buffer owned by it would take the user's unsaved edits with
//! it — the same lesson the terminal's PTY and the event drain each taught in Phase 1.

use std::path::{Path, PathBuf};

use freya::{code_editor::*, prelude::*};
use mf_engine::{files::Entry, proto::EngineCommand};

use crate::{Bridge, state::AppState, theme::Theme};

/// Indent per level of the tree, in pixels.
const INDENT: f32 = 14.;

/// The editor's font, deliberately *not* taken from the UI theme.
///
/// Both the buffer's line measurement and the rendered component must use the same values or the
/// cursor lands in the wrong column, so they are pinned here and read from both places rather
/// than passed separately. Code wants a monospace face at a size of its own regardless of what
/// the surrounding UI is set to; a proper editor-font setting is Phase 5's problem.
const EDITOR_FONT: &str = "JetBrains Mono";
const EDITOR_FONT_SIZE: f32 = 14.;

/// Grammars we ship. Everything else opens as plain text — the editor still gives a gutter,
/// selection, undo and save, it just does not colour anything.
///
/// Matched on extension rather than content: a shebang sniffer is more code and gets the answer
/// wrong on exactly the files where it matters (a `.py` with no shebang).
fn language_for(path: &Path) -> Option<EditorLanguage> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => EditorLanguage::new(tree_sitter_rust::LANGUAGE, tree_sitter_rust::HIGHLIGHTS_QUERY),
        "py" | "pyi" => {
            EditorLanguage::new(tree_sitter_python::LANGUAGE, tree_sitter_python::HIGHLIGHTS_QUERY)
        }
        "json" => {
            EditorLanguage::new(tree_sitter_json::LANGUAGE, tree_sitter_json::HIGHLIGHTS_QUERY)
        }
        "toml" => EditorLanguage::new(
            tree_sitter_toml_ng::LANGUAGE,
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY,
        ),
        "sk" => EditorLanguage::new(
            tree_sitter_skript::LANGUAGE,
            tree_sitter_skript::HIGHLIGHTS_QUERY,
        ),
        _ => return None,
    })
}

fn is_skript(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("sk"))
}

/// Spaces a leading tab is shown as. Four, to match the space-indented files it sits beside.
const TAB_WIDTH: usize = 4;

/// Expand leading tabs to spaces, reporting whether the file used any.
///
/// `\t` is a control character with no glyph, and the editor hands span text straight to Skia,
/// which draws it as a missing-glyph box. So a tab-indented line rendered as `▯▯code` rather than
/// as indentation. It only showed up sometimes because the editor has a second path for leading
/// whitespace that *does* blank it — but that path is only taken when the indent happens to differ
/// in colour from the token after it.
///
/// Only *leading* tabs are touched. A tab inside a string or between words is content, and
/// rewriting it would change what the file means.
fn expand_leading_tabs(text: &str) -> (String, bool) {
    if !text.lines().any(|l| l.starts_with('\t')) {
        return (text.to_owned(), false);
    }

    let expanded = text
        .split_inclusive('\n')
        .map(|line| {
            let indent = line.len() - line.trim_start_matches('\t').len();
            match indent {
                0 => line.to_owned(),
                n => " ".repeat(n * TAB_WIDTH) + &line[n..],
            }
        })
        .collect();
    (expanded, true)
}

/// Put the tabs back, for a file that had them.
///
/// Without this, opening a tab-indented file and saving it would rewrite every indented line —
/// an unrequested whole-file diff the user never asked for and would find in `git status` later.
fn restore_leading_tabs(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| {
            let spaces = line.len() - line.trim_start_matches(' ').len();
            // Leftover spaces beyond a whole number of tab stops stay as spaces, so an
            // odd indent is preserved rather than rounded to a tab boundary.
            let (tabs, rest) = (spaces / TAB_WIDTH, spaces % TAB_WIDTH);
            "\t".repeat(tabs) + &" ".repeat(rest) + &line[spaces..]
        })
        .collect()
}

/// One open file.
///
/// `data` is a signal so the editor can write to it, and it is created in the root scope so the
/// buffer outlives this panel. `PartialEq` compares by path: two tabs are the same tab when they
/// are the same file, and comparing buffer contents on every diff would be pointless work.
#[derive(Clone)]
pub struct Tab {
    pub path: PathBuf,
    pub data: State<CodeEditorData>,
    /// The file indented with tabs, which the buffer shows as spaces. Saving converts back.
    pub tabbed: bool,
}

impl PartialEq for Tab {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl Tab {
    /// Build a buffer for `content`, owned by the root scope.
    pub fn open(path: PathBuf, content: &str) -> Self {
        let language = language_for(&path);
        let (content, tabbed) = expand_leading_tabs(content);
        let mut data = CodeEditorData::new(Rope::from_str(&content), language);
        // `with_dark_code_editor()` on the global theme covers the *component* — background,
        // gutter, cursor — but the syntax colours live on the buffer, and `new` hardcodes
        // `EditorSyntaxTheme::default()`, which is the **light** one. Nothing wires the global
        // `code_editor_syntax` token to the buffer, so without this line the highlighting is
        // near-black text on a near-black background. Same trap as `markdown-code-editor`, one
        // layer further in, and it must come before `parse` because the parse bakes the colours
        // into the syntax blocks.
        // Skript brings its own palette — Sk-VSC's — because that grammar's captures are chosen
        // for which colour slot they land in, and the stock dark theme would put them on colours
        // that mean something else entirely.
        data.set_theme(if is_skript(&path) {
            crate::skript::syntax_theme()
        } else {
            EditorSyntaxTheme::dark()
        });
        // Required, and not done by `new`: the component renders one row per *syntax block*, and
        // those only exist after a parse. Without this the editor draws an empty pane over a
        // fully loaded buffer, which looks exactly like a file that failed to open.
        data.parse();
        data.measure(EDITOR_FONT_SIZE, EDITOR_FONT);
        Self { path, data: State::create_in_scope(data, Some(ScopeId::ROOT)), tabbed }
    }

    pub(crate) fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

#[derive(PartialEq)]
pub struct FilesPanel;

impl Component for FilesPanel {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();

        let root = state.workspace.read().clone();

        // Listed on open rather than at startup: most sessions never come here, and walking a
        // directory nobody is looking at is work for nothing. Re-requested when the workspace
        // changes, since the cached listing then belongs to a directory we have left.
        use_side_effect_with_deps(&root, {
            let cmd_tx = bridge.cmd_tx.clone();
            let listings = state.listings;
            move |root: &PathBuf| {
                if !listings.peek().contains_key(root) {
                    let _ = cmd_tx.send(EngineCommand::ListDir { path: root.clone() });
                }
            }
        });

        rect()
            .expanded()
            .direction(Direction::Horizontal)
            .background(theme.bg)
            .content(Content::flex())
            .child(
                // Fixed width. A flexed tree would resize as file names change length, and Freya
                // has no splitter here yet.
                rect()
                    .width(Size::px(300.))
                    .height(Size::fill())
                    .background(theme.surface)
                    .direction(Direction::Vertical)
                    // The search box is a fixed header; only what is below it scrolls, so the
                    // query stays on screen while the results are being read.
                    .content(Content::flex())
                    // The root of the tree *is* the workspace, so the control that changes it
                    // belongs at the top of the tree rather than only on a settings screen.
                    .child(
                        rect()
                            .width(Size::fill())
                            .direction(Direction::Horizontal)
                            .cross_align(Alignment::Center)
                            .spacing(theme.gap(6.))
                            .padding((theme.gap(8.), theme.gap(8.), 0., theme.gap(8.)))
                            // No path label here: the header already carries the workspace on
                            // every tab, and repeating it in a 300px column only crowds out the
                            // button.
                            .child(crate::workspace::choose_folder_button(
                                bridge.cmd_tx.clone(),
                                root.clone(),
                                "Open folder…",
                            )),
                    )
                    .child(crate::search::SearchBox)
                    .child(
                        rect().width(Size::fill()).height(Size::flex(1.)).child(
                            ScrollView::new().child(
                                rect()
                                    .width(Size::fill())
                                    .direction(Direction::Vertical)
                                    .padding(theme.gap(8.))
                                    // One column, two modes: a query replaces the tree with its
                                    // results rather than competing with it for width.
                                    .children(if state.search.read().query.is_empty() {
                                        tree_rows(&theme, &state, &bridge, &root, 0)
                                    } else {
                                        crate::search::result_rows(&theme, &state, &bridge)
                                    }),
                            ),
                        ),
                    ),
            )
            .child(rect().width(Size::flex(1.)).height(Size::fill()).child(EditorPane))
    }
}

/// Flatten the expanded parts of the tree into rows.
///
/// Recursion is over *expanded* directories only, so the cost is what is on screen rather than
/// what is on disk — which is what keeps this usable in a repository nobody has fully expanded.
fn tree_rows(
    theme: &Theme,
    state: &AppState,
    bridge: &Bridge,
    dir: &Path,
    depth: usize,
) -> Vec<Element> {
    let listings = state.listings.read();
    let Some(entries) = listings.get(dir) else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    for entry in entries {
        let expanded = state.expanded.read().contains(&entry.path);
        rows.push(tree_row(theme, state, bridge, entry, depth, expanded).into_element());
        if entry.is_dir && expanded {
            rows.extend(tree_rows(theme, state, bridge, &entry.path, depth + 1));
        }
    }
    rows
}

fn tree_row(
    theme: &Theme,
    state: &AppState,
    bridge: &Bridge,
    entry: &Entry,
    depth: usize,
    expanded: bool,
) -> impl IntoElement + use<> {
    let open_tab = state.tabs.read().iter().any(|t| t.path == entry.path);

    let press = {
        let (cmd_tx, entry) = (bridge.cmd_tx.clone(), entry.clone());
        let (mut expanded_set, listings) = (state.expanded, state.listings);
        let (tabs, mut active_tab) = (state.tabs, state.active_tab);
        move |_| {
            if entry.is_dir {
                let mut set = expanded_set.write();
                if !set.remove(&entry.path) {
                    set.insert(entry.path.clone());
                    // Fetched once and kept: re-listing on every expand would make the tree
                    // flicker and hammer the disk for a directory that has not changed.
                    if !listings.read().contains_key(&entry.path) {
                        let _ = cmd_tx.send(EngineCommand::ListDir { path: entry.path.clone() });
                    }
                }
                return;
            }

            // Already open: focus it rather than loading a second buffer over the first, which
            // would discard whatever is unsaved in the one already there.
            if let Some(i) = tabs.read().iter().position(|t| t.path == entry.path) {
                active_tab.set(i);
                return;
            }
            let _ = cmd_tx.send(EngineCommand::OpenFile { path: entry.path.clone() });
        }
    };

    let marker = if entry.is_dir {
        if expanded { "▾ " } else { "▸ " }
    } else {
        "  "
    };

    rect()
        .width(Size::fill())
        .padding((theme.gap(3.), theme.gap(4.)))
        .margin((0., 0., 0., depth as f32 * INDENT))
        .corner_radius(4.)
        .background(if open_tab { theme.surface_alt } else { theme.surface })
        .on_press(press)
        .child(
            label()
                .color(if entry.is_dir { theme.text } else { theme.text_dim })
                .font_size(theme.font_size - 1.)
                .text(format!("{marker}{}", entry.name)),
        )
}

#[derive(PartialEq)]
struct EditorPane;

impl Component for EditorPane {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();
        let a11y_id = use_a11y();

        let tabs = state.tabs.read().clone();
        let active = (*state.active_tab.read()).min(tabs.len().saturating_sub(1));

        let Some(tab) = tabs.get(active).cloned() else {
            return rect()
                .expanded()
                .main_align(Alignment::Center)
                .cross_align(Alignment::Center)
                .child(
                    label()
                        .color(theme.text_dim)
                        .text("Pick a file from the tree to open it here."),
                )
                .into_element();
        };

        let save = {
            let cmd_tx = bridge.cmd_tx.clone();
            let tab = tab.clone();
            move || {
                let content = tab.data.read().rope.to_string();
                // Indentation goes back the way it arrived, so opening a tab-indented file and
                // saving it does not rewrite every line of it.
                let content =
                    if tab.tabbed { restore_leading_tabs(&content) } else { content };
                // Deliberately does *not* clear the modified marker here. The write can still
                // fail — a read-only file, a full disk, a path the policy refuses — and a tab
                // that says "saved" over work only held in memory is how that work gets lost.
                // The drain clears it when `FileSaved` confirms the bytes landed.
                let _ = cmd_tx.send(EngineCommand::SaveFile {
                    path: tab.path.clone(),
                    content,
                });
            }
        };

        rect()
            .expanded()
            .direction(Direction::Vertical)
            .background(theme.bg)
            .content(Content::flex())
            .child(tab_bar(&theme, &state, &tabs, active))
            .child(
                // Keyed by path, and that key is load bearing. `Writable`'s `PartialEq` returns
                // `true` unconditionally, so `CodeEditor`'s derived `PartialEq` reports "same
                // props" even when it has been handed a completely different buffer — Freya then
                // skips the re-render and the pane keeps showing the previous file while the tab
                // bar says otherwise. Changing the key on the parent rebuilds the subtree, which
                // is what actually swaps the buffer. `KeyExt` is only implemented for primitive
                // elements, so the key goes here rather than on the component.
                rect().key(&tab.path).width(Size::fill()).height(Size::flex(1.)).child(
                    CodeEditor::new(tab.data, a11y_id)
                        .font_family(EDITOR_FONT)
                        .font_size(EDITOR_FONT_SIZE)
                        // Defaults to on, which puts a dot under every space of every indent.
                        .show_whitespace(false)
                        .a11y_auto_focus(true)
                        .on_pre_key_down(move |e: Event<KeyboardEventData>| {
                            e.stop_propagation();
                            let ctrl = e.modifiers.contains(Modifiers::CONTROL)
                                || e.modifiers.contains(Modifiers::META);
                            if ctrl && matches!(&e.key, Key::Character(c) if c == "s") {
                                save();
                                // Consumed, so the editor does not also treat it as text.
                                return false;
                            }
                            if let Key::Named(NamedKey::Tab) = &e.key {
                                e.prevent_default();
                            }
                            true
                        }),
                ),
            )
            .into_element()
    }
}

fn tab_bar(theme: &Theme, state: &AppState, tabs: &[Tab], active: usize) -> impl IntoElement + use<> {
    let rows: Vec<Element> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let selected = i == active;
            // Read from the buffer itself rather than tracked separately — the editor owns the
            // history, so it is the only thing that can be right about this.
            let modified = tab.data.read().is_edited();

            let focus = {
                let mut active_tab = state.active_tab;
                move |_| active_tab.set(i)
            };
            let close = {
                let (mut tabs_state, mut active_tab) = (state.tabs, state.active_tab);
                move |e: Event<PressEventData>| {
                    // Otherwise the row underneath also fires and re-selects a tab that is gone.
                    e.stop_propagation();
                    tabs_state.write().remove(i);
                    let len = tabs_state.read().len();
                    active_tab.set(active_tab.read().min(len.saturating_sub(1)));
                }
            };

            rect()
                .direction(Direction::Horizontal)
                .cross_align(Alignment::Center)
                .spacing(theme.gap(6.))
                .padding((theme.gap(6.), theme.gap(10.)))
                .background(if selected { theme.bg } else { theme.surface })
                .on_press(focus)
                .child(
                    label()
                        .color(if selected { theme.text } else { theme.text_dim })
                        .font_size(theme.font_size - 1.)
                        // A dot, not just a colour: unsaved work is not something to signal on
                        // hue alone.
                        .text(if modified {
                            format!("● {}", tab.name())
                        } else {
                            tab.name()
                        }),
                )
                .child(
                    label()
                        .color(theme.text_dim)
                        .font_size(theme.font_size - 1.)
                        .text("✕")
                        .on_press(close),
                )
                .into_element()
        })
        .collect();

    // Horizontal scroll rather than wrapping: Freya has no flex wrap, and a second row of tabs
    // would push the editor down every time one more file is opened.
    ScrollView::new()
        .direction(Direction::Horizontal)
        .height(Size::px(36.))
        .child(rect().direction(Direction::Horizontal).spacing(theme.gap(2.)).children(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leading_tabs_become_spaces_and_come_back() {
        let original = "on join:\n\tsend \"hi\"\n\t\tstop\n";
        let (shown, tabbed) = expand_leading_tabs(original);

        assert!(tabbed);
        // A tab has no glyph, so what reaches the renderer must be spaces.
        assert!(!shown.contains('\t'), "a tab survived into the buffer: {shown:?}");
        assert_eq!(shown, "on join:\n    send \"hi\"\n        stop\n");
        // And saving must not rewrite every indented line of the user's file.
        assert_eq!(restore_leading_tabs(&shown), original);
    }

    #[test]
    fn a_space_indented_file_is_left_completely_alone() {
        // The common case, and the one where touching anything would be a gratuitous diff.
        let original = "on join:\n    send \"hi\"\n";
        let (shown, tabbed) = expand_leading_tabs(original);
        assert!(!tabbed, "a space-indented file must not be marked as tabbed");
        assert_eq!(shown, original);
    }

    #[test]
    fn tabs_that_are_not_indentation_are_content_and_stay() {
        // A tab inside a string is data. Rewriting it changes what the file means.
        let original = "set {x} to \"a\tb\"\n";
        let (shown, tabbed) = expand_leading_tabs(original);
        assert!(!tabbed);
        assert_eq!(shown, original);
    }

    #[test]
    fn an_odd_indent_is_preserved_rather_than_rounded() {
        // Six spaces is a tab and two spaces, not two tabs — rounding would silently re-indent
        // a line, and Skript cares about indentation.
        assert_eq!(restore_leading_tabs("      x\n"), "\t  x\n");
    }

    #[test]
    fn known_extensions_get_a_grammar_and_the_rest_open_as_plain_text() {
        assert!(language_for(Path::new("src/main.rs")).is_some());
        assert!(language_for(Path::new("app.py")).is_some());
        assert!(language_for(Path::new("Cargo.toml")).is_some());
        assert!(language_for(Path::new("package.json")).is_some());

        // Not shipping a grammar must degrade to a working plain-text editor, never to a refusal
        // to open the file.
        assert!(language_for(Path::new("notes.md")).is_none());
        assert!(language_for(Path::new("Makefile")).is_none());
        assert!(language_for(Path::new("weird.RS")).is_some(), "extension match is case-insensitive");
    }
}
