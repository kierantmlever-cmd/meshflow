//! Provider settings.
//!
//! This is the only screen that touches a secret, so two rules shape it:
//!
//! 1. The key field is masked, lives in its own signal, and is cleared the instant it is handed
//!    to the engine. It is never part of the entry struct that gets cloned around the form.
//! 2. The engine sends back `has_key`, never the key. There is no code path that renders a stored
//!    key, because there is no code path that fetches one — a key goes in and never comes out.
//!
//! Renaming is deliberately not offered. The entry name *is* the keychain account, so a rename
//! would orphan the stored key and leave the new entry silently unauthenticated. Editing shows
//! the name as text; changing it means creating a new provider and deleting the old one.

use freya::prelude::*;
use mf_engine::{
    config::ProviderEntry,
    proto::{EngineCommand, ProviderSummary},
    provider::{ModelInfo, ProviderKind},
    secrecy::SecretString,
};

use crate::{Bridge, state::AppState, theme::Theme};

/// Ready-made endpoints. Every one of these is otherwise a URL the user has to go and look up,
/// and getting it subtly wrong produces a 404 that reads like an auth failure.
///
/// **Deliberately no default model.** A preset that names one is wrong within the month: this
/// list shipped with `deepseek-chat`, and DeepSeek's own catalogue advertises `deepseek-v4-flash`
/// and `deepseek-v4-pro`. The endpoint is the durable half and the model is not, so the preset
/// fills in the URL and Fetch supplies the truth.
const PRESETS: &[Preset] = &[
    Preset::new("OpenAI", ProviderKind::OpenAi, "https://api.openai.com/v1", true),
    Preset::new("Anthropic", ProviderKind::Anthropic, "https://api.anthropic.com", true),
    Preset::new("OpenRouter", ProviderKind::OpenAi, "https://openrouter.ai/api/v1", true),
    Preset::new("DeepSeek", ProviderKind::OpenAi, "https://api.deepseek.com", true),
    Preset::new("Groq", ProviderKind::OpenAi, "https://api.groq.com/openai/v1", true),
    // Ollama serves OpenAI's wire format on `/v1`, so it uses that codec rather than a
    // near-identical copy of it, and wants no key.
    Preset::new("Ollama", ProviderKind::OpenAi, "http://localhost:11434/v1", false),
];

/// Vendor namespaces kept when a catalogue is too long to scan.
///
/// Matching the *namespace* rather than model names is what stops this going stale: whatever
/// OpenAI ships next appears under `openai/` the day it launches, with nothing to update here. A
/// list of model names written today would be wrong within the month.
const FLAGSHIP_VENDORS: &[&str] =
    &["openai/", "anthropic/", "deepseek/", "google/", "qwen/", "minimax/"];

/// Above this many models the picker filters to [`FLAGSHIP_VENDORS`] by default. Aggregators
/// return several hundred; a direct provider returns a few dozen and needs no filtering.
const CLUTTER_THRESHOLD: usize = 40;

/// Shown while a fetch is in flight, and retracted once the picker has something to show.
const FETCHING: &str = "Fetching models…";

struct Preset {
    name: &'static str,
    kind: ProviderKind,
    base_url: &'static str,
    needs_key: bool,
}

impl Preset {
    const fn new(
        name: &'static str,
        kind: ProviderKind,
        base_url: &'static str,
        needs_key: bool,
    ) -> Self {
        Self { name, kind, base_url, needs_key }
    }
}

/// The codecs that actually exist. Google is absent on purpose — there is no Google codec yet,
/// and offering it would only produce requests it cannot parse.
const KINDS: &[(ProviderKind, &str)] =
    &[(ProviderKind::OpenAi, "OpenAI-compatible"), (ProviderKind::Anthropic, "Anthropic")];

#[derive(PartialEq)]
pub struct SettingsView;

impl Component for SettingsView {
    fn render(&self) -> impl IntoElement {
        let bridge = use_consume::<Bridge>();
        let theme = use_consume::<Theme>();
        let state = use_consume::<AppState>();

        let mut name = use_state(String::new);
        let mut base_url = use_state(String::new);
        let mut model = use_state(String::new);
        // The context window the provider reported, paired with the model id it belongs to. The
        // pairing is what makes it self-invalidating: type over a picked id and the window no
        // longer matches, so it is dropped rather than saved against the wrong model.
        let mut context_window = use_state(|| None::<(String, u32)>);
        let window_for = move |id: &str| match &*context_window.read() {
            Some((picked, window)) if picked == id => Some(*window),
            _ => None,
        };
        // Kept apart from everything else so no struct holding form data ever holds the secret.
        let mut key = use_state(String::new);
        let mut kind = use_state(ProviderKind::default);
        let mut needs_key = use_state(|| true);
        // `Some(name)` while editing an existing entry, `None` while creating one.
        let mut editing = use_state(|| None::<String>);
        let mut notice = use_state(String::new);
        let mut picking = use_state(|| false);
        // True once the switch has been flipped but before the warning has been accepted. The
        // flip alone must not arm it: this is the one control in the app that turns off every
        // other consent screen.
        let arming = use_state(|| false);
        let flagships_only = use_state(|| true);

        // The list is engine-owned; ask for it once on open rather than caching a copy that can
        // drift from what is on disk.
        use_hook({
            let cmd_tx = bridge.cmd_tx.clone();
            move || {
                let _ = cmd_tx.send(EngineCommand::RequestProviders);
            }
        });

        let mut reset = move || {
            name.set(String::new());
            base_url.set(String::new());
            model.set(String::new());
            context_window.set(None);
            key.set(String::new());
            kind.set(ProviderKind::default());
            needs_key.set(true);
            editing.set(None);
            notice.set(String::new());
            // Closed rather than left open: the list belongs to the provider being replaced.
            picking.set(false);
        };

        // The entry as the form currently describes it, for commands that act on a draft the
        // user has not saved yet.
        let draft_entry = move || ProviderEntry {
            name: name.read().trim().to_owned(),
            kind: *kind.read(),
            base_url: base_url.read().trim().to_owned(),
            context_window: window_for(model.read().trim()),
            model: model.read().trim().to_owned(),
            needs_key: *needs_key.read(),
            org_id: None,
            headers: Default::default(),
        };

        let mut load_preset = move |p: &Preset| {
            reset();
            name.set(p.name.to_lowercase());
            base_url.set(p.base_url.to_owned());
            kind.set(p.kind);
            needs_key.set(p.needs_key);
            // Model deliberately left blank — see PRESETS.
            notice.set("Add your key, then Fetch to list this provider's models.".into());
        };

        let load_entry = move |s: &ProviderSummary| {
            reset();
            name.set(s.entry.name.clone());
            base_url.set(s.entry.base_url.clone());
            model.set(s.entry.model.clone());
            context_window.set(s.entry.context_window.map(|w| (s.entry.model.clone(), w)));
            kind.set(s.entry.kind);
            needs_key.set(s.entry.needs_key);
            editing.set(Some(s.entry.name.clone()));
        };

        let save = {
            let cmd_tx = bridge.cmd_tx.clone();
            let providers = state.providers;
            move |_| {
                let (entry_name, url, model_id) = (
                    name.read().trim().to_owned(),
                    base_url.read().trim().to_owned(),
                    model.read().trim().to_owned(),
                );
                if entry_name.is_empty() || url.is_empty() || model_id.is_empty() {
                    notice.set("Name, base URL and model are all required.".into());
                    return;
                }

                let secret = key.read().trim().to_owned();
                let stored = providers
                    .read()
                    .iter()
                    .any(|p| p.entry.name == entry_name && p.has_key);
                // Catching this here turns a confusing 401 later into a sentence now.
                if *needs_key.read() && secret.is_empty() && !stored {
                    notice.set("This provider needs an API key.".into());
                    return;
                }

                let _ = cmd_tx.send(EngineCommand::SaveProvider {
                    entry: ProviderEntry {
                        name: entry_name.clone(),
                        kind: *kind.read(),
                        base_url: url,
                        context_window: window_for(&model_id),
                        model: model_id,
                        needs_key: *needs_key.read(),
                        org_id: None,
                        headers: Default::default(),
                    },
                    // An empty box means "leave the stored key alone", so editing a model name
                    // does not require retyping the key.
                    key: (!secret.is_empty()).then(|| SecretString::from(secret)),
                });
                // Gone from the UI the moment it is handed over.
                key.set(String::new());
                editing.set(Some(entry_name));
                notice.set("Saved.".into());
            }
        };

        let fetch = {
            let cmd_tx = bridge.cmd_tx.clone();
            move |_| {
                let entry = draft_entry();
                if entry.base_url.is_empty() {
                    notice.set("Set a base URL first — that is where the list comes from.".into());
                    return;
                }
                // Named after the endpoint, not the entry: the user may not have named it yet,
                // and an empty keychain account would silently look up the wrong key.
                let secret = key.read().trim().to_owned();
                let _ = cmd_tx.send(EngineCommand::ListModels {
                    entry,
                    key: (!secret.is_empty()).then(|| SecretString::from(secret)),
                });
                notice.set(FETCHING.into());
                picking.set(true);
            }
        };

        let providers = state.providers.read().clone();
        let keychain = *state.keychain.read();

        // Only offer a catalogue that belongs to the provider on screen. A stale list from the
        // previously selected provider would look authoritative and be wrong.
        let catalogue = state.models.read().clone();
        let fetched: Vec<ModelInfo> = if catalogue.provider == *name.read().trim() {
            catalogue.models
        } else {
            Vec::new()
        };
        let engine_error = state.last_error.read().clone();

        // The picker appearing is itself the success signal; leaving "Fetching models…" standing
        // beside a populated list reads as a request that never came back.
        let notice_text = {
            let current = notice.read().clone();
            if current == FETCHING && !fetched.is_empty() { String::new() } else { current }
        };

        ScrollView::new().child(
            rect()
                .width(Size::fill())
                .direction(Direction::Vertical)
                .spacing(theme.gap(18.))
                .padding((theme.gap(20.), theme.gap(24.)))
                // First on the screen: which directory an agent may touch matters more than which
                // model it talks to, and the answer should be visible before anything is typed.
                .child(crate::workspace::WorkspaceSection)
                .child(heading(&theme, "Approvals"))
                .child(approvals_section(
                    &theme,
                    &bridge,
                    *state.auto_approve.read(),
                    arming,
                    &state.workspace.read(),
                ))
                .child(heading(&theme, "Providers"))
                .map((!keychain).then_some(()), |root, ()| {
                    // Said before a key is typed, not after it fails to save.
                    root.child(warning(
                        &theme,
                        "No OS keychain is available, so API keys cannot be stored. On Linux, run \
                         a Secret Service provider such as gnome-keyring or kwallet.",
                    ))
                })
                .child(if providers.is_empty() {
                    label()
                        .color(theme.text_dim)
                        .text("No providers configured yet. Pick a preset below to add one.")
                        .into_element()
                } else {
                    rect()
                        .width(Size::fill())
                        .direction(Direction::Vertical)
                        .spacing(theme.gap(8.))
                        .children(
                            providers
                                .iter()
                                .map(|p| {
                                    provider_row(&theme, p, &bridge, load_entry).into_element()
                                })
                                .collect::<Vec<_>>(),
                        )
                        .into_element()
                })
                .child(heading(
                    &theme,
                    match editing.read().as_deref() {
                        Some(n) => format!("Edit {n}"),
                        None => "Add a provider".to_owned(),
                    },
                ))
                .child(
                    // Freya has no flex wrapping, so a narrow window scrolls the presets rather
                    // than clipping the last of them off the edge. The explicit height is load
                    // bearing: a ScrollView with none takes every remaining pixel of the column,
                    // which pushed the whole form below the fold.
                    ScrollView::new().direction(Direction::Horizontal).height(Size::px(52.)).child(
                        rect()
                            .direction(Direction::Horizontal)
                            .spacing(theme.gap(6.))
                            .children(
                                PRESETS
                                    .iter()
                                    .map(|p| {
                                        Button::new()
                                            .on_press(move |_| load_preset(p))
                                            .child(p.name)
                                            .into_element()
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                    ),
                )
                .child(match editing.read().clone() {
                    // The name is the keychain account, so it is fixed once an entry exists.
                    Some(existing) => field(
                        &theme,
                        "Name",
                        label().color(theme.text_dim).text(existing).into_element(),
                    ),
                    None => field(
                        &theme,
                        "Name",
                        Input::new(name).width(Size::fill()).placeholder("openai").into_element(),
                    ),
                })
                .child(field(
                    &theme,
                    "API format",
                    Select::new()
                        .selected_item(kind_label(*kind.read()))
                        .children(KINDS.iter().map(|(k, text)| {
                            MenuItem::new()
                                .selected(*k == *kind.read())
                                .on_press(move |_| kind.set(*k))
                                .child(*text)
                                .into_element()
                        }))
                        .into_element(),
                ))
                .child(field(
                    &theme,
                    "Base URL",
                    Input::new(base_url).width(Size::fill()).placeholder("https://api.openai.com/v1").into_element(),
                ))
                .child(field(
                    &theme,
                    "Model",
                    rect()
                        .width(Size::fill())
                        .direction(Direction::Horizontal)
                        .cross_align(Alignment::Center)
                        .spacing(theme.gap(8.))
                        .content(Content::flex())
                        // Stays free text. The picker is a convenience on top of it, never a
                        // gate — a model released this morning must still be typeable.
                        .child(Input::new(model).width(Size::flex(1.)).placeholder("model id"))
                        .child(Button::new().on_press(fetch).child(if fetched.is_empty() {
                            "Fetch"
                        } else {
                            "Refresh"
                        }))
                        .into_element(),
                ))
                .map((picking() && !fetched.is_empty()).then_some(()), |root, ()| {
                    root.child(model_picker(
                        &theme,
                        &fetched,
                        model,
                        flagships_only,
                        move |chosen, window| {
                            context_window.set(window.map(|w| (chosen.clone(), w)));
                            model.set(chosen);
                            picking.set(false);
                        },
                    ))
                })
                .child(field(
                    &theme,
                    "Needs an API key",
                    Switch::new()
                        .toggled(*needs_key.read())
                        .on_toggle(move |_| {
                            let next = !*needs_key.read();
                            needs_key.set(next);
                        })
                        .into_element(),
                ))
                .map((*needs_key.read()).then_some(()), |root, ()| {
                    root.child(field(
                        &theme,
                        "API key",
                        rect()
                            .width(Size::fill())
                            .direction(Direction::Vertical)
                            .spacing(theme.gap(4.))
                            .child(
                                Input::new(key)
                                    .mode(InputMode::new_password())
                                    .placeholder(match editing.read().as_deref() {
                                        Some(_) => "leave blank to keep the stored key",
                                        None => "sk-…",
                                    }),
                            )
                            .child(
                                label()
                                    .color(theme.text_dim)
                                    .font_size(theme.font_size - 3.)
                                    .text(
                                        "Stored in the OS keychain. Never written to config.toml, \
                                         never sent to a model, never shown again.",
                                    ),
                            )
                            .into_element(),
                    ))
                })
                .child(
                    rect()
                        .direction(Direction::Horizontal)
                        .spacing(theme.gap(8.))
                        .cross_align(Alignment::Center)
                        .child(Button::new().filled().on_press(save).child("Save"))
                        .child(Button::new().on_press(move |_| reset()).child("Clear"))
                        .map((!notice_text.is_empty()).then_some(()), |row, ()| {
                            row.child(label().color(theme.text_dim).text(notice_text.clone()))
                        }),
                )
                // A failed fetch or save reported only in the chat transcript is a failure the
                // user never sees, because they are standing right here when it happens.
                .map((!engine_error.is_empty()).then_some(()), |root, ()| {
                    root.child(warning(&theme, engine_error.clone()))
                })
                .child(
                    label()
                        .color(theme.text_dim)
                        .font_size(theme.font_size - 3.)
                        .text(format!(
                            "Settings live in {}",
                            mf_engine::config::Config::path()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|_| "config.toml".into())
                        )),
                ),
        )
    }
}

/// The list of models to offer, given the vendor filter and whatever is typed in the field.
///
/// Returns `(models, filtered_out)`. Never returns empty while the catalogue has entries — a
/// filter that hides everything is indistinguishable from a fetch that failed, so it falls back
/// to the unfiltered list rather than showing a blank panel.
fn visible_models<'a>(
    all: &'a [ModelInfo],
    flagships_only: bool,
    typed: &str,
) -> (Vec<&'a ModelInfo>, usize) {
    let vendor_pass: Vec<&ModelInfo> = if flagships_only && all.len() > CLUTTER_THRESHOLD {
        let kept: Vec<&ModelInfo> = all
            .iter()
            .filter(|m| {
                let id = m.id.to_ascii_lowercase();
                FLAGSHIP_VENDORS.iter().any(|v| id.starts_with(v))
            })
            .collect();
        if kept.is_empty() { all.iter().collect() } else { kept }
    } else {
        all.iter().collect()
    };
    let hidden = all.len() - vendor_pass.len();

    // Typing in the field narrows the list, which is what makes a 300-entry catalogue usable.
    let needle = typed.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return (vendor_pass, hidden);
    }
    // An id that exactly matches a model is the *current* selection, not a search. Treating it as
    // one filters the picker down to the model you already have and hides every alternative —
    // backwards, since changing model is the only reason to open the picker.
    if all.iter().any(|m| m.id.to_ascii_lowercase() == needle) {
        return (vendor_pass, hidden);
    }
    let matched: Vec<&ModelInfo> =
        vendor_pass.iter().copied().filter(|m| m.id.to_ascii_lowercase().contains(&needle)).collect();
    // A half-typed id that matches nothing yet should not blank the panel.
    if matched.is_empty() { (vendor_pass, hidden) } else { (matched, hidden) }
}

/// Height for the model list: fits the rows, capped so a long catalogue still leaves Save on
/// screen. Row height is the button plus its gap, measured against the rendered UI.
fn picker_height(rows: usize) -> f32 {
    const ROW: f32 = 30.;
    const MAX: f32 = 220.;
    (rows as f32 * ROW).clamp(ROW, MAX)
}

fn model_picker(
    theme: &Theme,
    all: &[ModelInfo],
    model: State<String>,
    mut flagships_only: State<bool>,
    // Takes the reported window with the id: the engine sizes its history budget from it, and
    // this list is the only place it is ever known.
    choose: impl FnMut(String, Option<u32>) + Clone + 'static,
) -> impl IntoElement {
    let filtering = all.len() > CLUTTER_THRESHOLD;
    let (visible, hidden) = visible_models(all, *flagships_only.read(), &model.read());

    let rows: Vec<Element> = visible
        .iter()
        .map(|m| {
            let id = m.id.clone();
            let window = m.context_window;
            let mut choose = choose.clone();
            let context = m
                .context_window
                .map(|c| {
                    if c >= 1_000_000 {
                        format!("{}M ctx", c / 1_000_000)
                    } else {
                        format!("{}k ctx", c / 1_000)
                    }
                })
                .unwrap_or_default();

            rect()
                .width(Size::fill())
                .direction(Direction::Horizontal)
                .cross_align(Alignment::Center)
                .content(Content::flex())
                .child(
                    Button::new()
                        .on_press(move |_| choose(id.clone(), window))
                        .child(m.id.clone())
                        .into_element(),
                )
                .child(
                    label()
                        .color(theme.text_dim)
                        .font_size(theme.font_size - 3.)
                        .text(context),
                )
                .into_element()
        })
        .collect();

    rect()
        .width(Size::fill())
        .direction(Direction::Horizontal)
        .spacing(theme.gap(12.))
        .child(rect().width(Size::px(130.)))
        .child(
            rect()
                .width(Size::px(520.))
                .direction(Direction::Vertical)
                .spacing(theme.gap(6.))
                .padding(theme.gap(10.))
                .corner_radius(8.)
                .background(theme.surface)
                .child(
                    rect()
                        .width(Size::fill())
                        .direction(Direction::Horizontal)
                        .cross_align(Alignment::Center)
                        .spacing(theme.gap(8.))
                        .child(
                            label()
                                .color(theme.text_dim)
                                .font_size(theme.font_size - 3.)
                                .text(format!("{} of {} models", visible.len(), all.len())),
                        )
                        .map(filtering.then_some(()), |row, ()| {
                            row.child(
                                Switch::new()
                                    .toggled(*flagships_only.read())
                                    .on_toggle(move |_| {
                                        let next = !*flagships_only.read();
                                        flagships_only.set(next);
                                    })
                                    .into_element(),
                            )
                            .child(
                                label()
                                    .color(theme.text_dim)
                                    .font_size(theme.font_size - 3.)
                                    .text(if hidden > 0 {
                                        format!("major vendors only ({hidden} hidden)")
                                    } else {
                                        "major vendors only".to_owned()
                                    }),
                            )
                        }),
                )
                .child(
                    // Bounded, but only as tall as it needs: a catalogue of several hundred would
                    // push Save off the bottom of the window, while a two-model list padded to a
                    // fixed height reads as a panel that failed to load the rest.
                    ScrollView::new()
                        .direction(Direction::Vertical)
                        .height(Size::px(picker_height(visible.len())))
                        .child(
                            rect()
                                .width(Size::fill())
                                .direction(Direction::Vertical)
                                .spacing(theme.gap(2.))
                                .children(rows),
                        ),
                ),
        )
}

fn kind_label(kind: ProviderKind) -> String {
    KINDS
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, text)| (*text).to_owned())
        .unwrap_or_else(|| format!("{kind:?}"))
}

fn provider_row(
    theme: &Theme,
    summary: &ProviderSummary,
    bridge: &Bridge,
    mut load_entry: impl FnMut(&ProviderSummary) + 'static,
) -> impl IntoElement {
    let name = summary.entry.name.clone();
    let owned = summary.clone();

    let use_it = {
        let (cmd_tx, name) = (bridge.cmd_tx.clone(), name.clone());
        move |_| {
            let _ = cmd_tx.send(EngineCommand::SetActiveProvider { name: name.clone() });
        }
    };
    let delete = {
        let (cmd_tx, name) = (bridge.cmd_tx.clone(), name.clone());
        move |_| {
            let _ = cmd_tx.send(EngineCommand::DeleteProvider { name: name.clone() });
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
        // Without this the `flex(1.)` below is inert, the name column takes the whole row, and
        // the badge and buttons get squeezed into a one-character-wide column.
        .content(Content::flex())
        .child(
            rect()
                .width(Size::flex(1.))
                .direction(Direction::Vertical)
                .spacing(theme.gap(2.))
                .child(
                    label()
                        .color(if summary.active { theme.accent } else { theme.text })
                        .text(if summary.active {
                            format!("{name}  ·  in use")
                        } else {
                            name.clone()
                        }),
                )
                .child(
                    label()
                        .color(theme.text_dim)
                        .font_size(theme.font_size - 3.)
                        .text(format!("{}  ·  {}", summary.entry.model, summary.entry.base_url)),
                ),
        )
        .child(
            label()
                .color(if summary.entry.needs_key && !summary.has_key {
                    theme.danger
                } else {
                    theme.text_dim
                })
                .font_size(theme.font_size - 3.)
                .text(match (summary.entry.needs_key, summary.has_key) {
                    (true, true) => "key stored",
                    (true, false) => "no key",
                    (false, _) => "no key needed",
                }),
        )
        .map((!summary.active).then_some(()), |row, ()| {
            row.child(Button::new().on_press(use_it).child("Use"))
        })
        .child(Button::new().on_press(move |_| load_entry(&owned)).child("Edit"))
        .child(Button::new().on_press(delete).child("Delete"))
}

/// The auto-approve control.
///
/// Arming it takes two deliberate acts — flip, then read, then confirm — and disarming takes one,
/// because the asymmetry is the point: the safe direction should never be harder than the
/// dangerous one. The warning names what is actually being given up *and* what is not, since a
/// warning that overstates the danger gets clicked through as fast as one that understates it.
fn approvals_section(
    theme: &Theme,
    bridge: &Bridge,
    on: bool,
    mut arming: State<bool>,
    workspace: &std::path::Path,
) -> impl IntoElement {
    // Plain function of the value, not an event handler factory: the buttons and the switch all
    // route through this one line, so there is a single place the mode is ever changed.
    let send = {
        let cmd_tx = bridge.cmd_tx.clone();
        move |value: bool| {
            let _ = cmd_tx.send(EngineCommand::SetAutoApprove(value));
            arming.set(false);
        }
    };

    rect()
        .width(Size::fill())
        .direction(Direction::Vertical)
        .spacing(theme.gap(10.))
        .child(field(
            theme,
            "Auto-approve",
            rect()
                .direction(Direction::Horizontal)
                .cross_align(Alignment::Center)
                .spacing(theme.gap(10.))
                .child(Switch::new().toggled(on || *arming.read()).on_toggle({
                    let mut send = send.clone();
                    move |_| {
                        // Off is immediate. On only opens the warning below — the flip itself
                        // arms nothing.
                        if on {
                            send(false);
                        } else {
                            let next = !*arming.read();
                            arming.set(next);
                        }
                    }
                }))
                .child(
                    label()
                        .color(if on { theme.danger } else { theme.text_dim })
                        .font_size(theme.font_size - 1.)
                        // Three states, not two. While arming, the switch reads as on and the
                        // mode is not — saying "Off" there is the kind of small lie that makes
                        // someone stop trusting the indicator entirely.
                        .text(match (on, *arming.read()) {
                            (true, _) => "ON — tool calls are running without asking",
                            (false, true) => "Not on yet — read the warning and confirm below",
                            (false, false) => "Off — every destructive call stops for your approval",
                        }),
                )
                .into_element(),
        ))
        .map((!on && *arming.read()).then_some(()), |root, ()| {
            root.child(
                rect()
                    .width(Size::fill())
                    .direction(Direction::Vertical)
                    .spacing(theme.gap(10.))
                    .padding(theme.gap(12.))
                    .corner_radius(8.)
                    .background(theme.surface)
                    .child(
                        label().color(theme.danger).font_size(theme.font_size + 1.).text(
                            "Turning this on means agents overwrite files and run shell commands \
                             with no prompt and no chance to say no.",
                        ),
                    )
                    .child(label().color(theme.text).text(format!(
                        "Still true with it on: agents stay inside {}, credentials, SSH keys and \
                         .env files are still refused, and every call is still written to the \
                         audit log — marked as having run unattended.\n\nIt turns itself off when \
                         you close MeshFlow.",
                        workspace.display(),
                    )))
                    .child(
                        rect()
                            .direction(Direction::Horizontal)
                            .spacing(theme.gap(8.))
                            .child(Button::new().on_press({
                                let mut send = send.clone();
                                move |_| send(false)
                            }).child("Cancel"))
                            // Named after what it does, not "OK" — the button someone clicks by
                            // reflex should say which way it goes.
                            .child(
                                Button::new()
                                    .filled()
                                    .on_press({
                                        let mut send = send.clone();
                                        move |_| send(true)
                                    })
                                    .child("Approve everything automatically"),
                            ),
                    ),
            )
        })
}

fn heading(theme: &Theme, text: impl Into<String>) -> impl IntoElement {
    label().color(theme.text).font_size(theme.font_size + 3.).text(text.into())
}

fn warning(theme: &Theme, text: impl Into<String>) -> impl IntoElement {
    rect()
        .width(Size::fill())
        .padding(theme.gap(10.))
        .corner_radius(8.)
        .background(theme.surface)
        .child(label().color(theme.danger).text(text.into()))
}


/// One labelled form row. Fixed-width label so the controls line up down the column, and a capped
/// control column — a base URL stretched across a 2560px monitor is harder to read, not easier.
fn field(theme: &Theme, name: &str, control: Element) -> impl IntoElement {
    rect()
        .width(Size::fill())
        .direction(Direction::Horizontal)
        .cross_align(Alignment::Center)
        .spacing(theme.gap(12.))
        .child(
            rect()
                .width(Size::px(130.))
                .child(label().color(theme.text_dim).text(name.to_owned())),
        )
        .child(rect().width(Size::px(520.)).child(control))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue(ids: &[&str]) -> Vec<ModelInfo> {
        ids.iter().map(|id| ModelInfo { id: (*id).to_owned(), context_window: None }).collect()
    }

    /// A long catalogue padded with entries from vendors the filter drops.
    fn big() -> Vec<ModelInfo> {
        let mut ids: Vec<String> =
            (0..CLUTTER_THRESHOLD).map(|i| format!("somevendor/filler-{i}")).collect();
        ids.push("openai/gpt-5.6-sol".into());
        ids.push("anthropic/claude-opus-5".into());
        ids.push("deepseek/deepseek-coder".into());
        catalogue(&ids.iter().map(String::as_str).collect::<Vec<_>>())
    }

    #[test]
    fn short_catalogues_are_never_filtered() {
        // A direct provider returns a few dozen models, all of them relevant. Filtering there
        // would hide the only models the key can actually reach.
        let all = catalogue(&["gpt-5.6-sol", "gpt-5.6-terra", "o4"]);
        let (visible, hidden) = visible_models(&all, true, "");
        assert_eq!(visible.len(), 3);
        assert_eq!(hidden, 0);
    }

    #[test]
    fn long_catalogues_keep_only_the_major_vendors() {
        let all = big();
        let (visible, hidden) = visible_models(&all, true, "");
        assert_eq!(visible.len(), 3, "the three vendor-namespaced models survive");
        assert_eq!(hidden, CLUTTER_THRESHOLD);
        assert!(visible.iter().any(|m| m.id == "openai/gpt-5.6-sol"));
    }

    #[test]
    fn turning_the_filter_off_shows_everything() {
        let all = big();
        let (visible, _) = visible_models(&all, false, "");
        assert_eq!(visible.len(), all.len());
    }

    #[test]
    fn a_filter_that_matches_nothing_falls_back_to_the_whole_list() {
        // Otherwise a catalogue from a vendor we do not list — or a namespace scheme that
        // changes — renders an empty panel that looks exactly like a failed fetch.
        let all = catalogue(
            &(0..CLUTTER_THRESHOLD + 1).map(|i| format!("mistral/model-{i}")).collect::<Vec<_>>()
                .iter().map(String::as_str).collect::<Vec<_>>(),
        );
        let (visible, hidden) = visible_models(&all, true, "");
        assert_eq!(visible.len(), all.len(), "never blank the panel");
        assert_eq!(hidden, 0);
    }

    #[test]
    fn typing_narrows_the_list() {
        let all = big();
        let (visible, _) = visible_models(&all, true, "opus");
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "anthropic/claude-opus-5");
    }

    #[test]
    fn the_currently_selected_model_does_not_filter_out_the_alternatives() {
        // Opening the picker for a saved provider puts its current model in the field. Treating
        // that as a search term shows you the one model you already have.
        let all = big();
        let (visible, _) = visible_models(&all, true, "anthropic/claude-opus-5");
        assert_eq!(visible.len(), 3, "an exact match is a selection, not a query");
    }

    #[test]
    fn a_half_typed_id_does_not_blank_the_list() {
        // The field holds the *current* model id, which often matches nothing in a freshly
        // fetched catalogue. Blanking the picker at that moment hides it exactly when opened.
        let all = big();
        let (visible, _) = visible_models(&all, true, "zzz-no-such-model");
        assert_eq!(visible.len(), 3);
    }

    #[test]
    fn the_vendor_list_matches_namespaces_not_model_names() {
        // The whole point: a model nobody had heard of when this was written still shows up,
        // because it arrives under a namespace that does not change.
        let all = big();
        let (visible, _) = visible_models(&all, true, "");
        assert!(
            visible.iter().any(|m| m.id.contains("gpt-5.6-sol")),
            "an unreleased-at-authoring-time model must pass the filter"
        );
    }
}
