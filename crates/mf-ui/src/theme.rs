//! Colours and metrics. Dark gray + teal by default; every field is user-customisable and
//! persisted (Phase 5 wires the settings UI — this is the shape it writes to).

use freya::{
    code_editor::CodeEditorThemeExt,
    prelude::{Color, Theme as FreyaTheme, dark_theme},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub bg: Color,
    pub surface: Color,
    pub surface_alt: Color,
    pub border: Color,
    pub text: Color,
    pub text_dim: Color,
    pub accent: Color,
    pub danger: Color,
    /// Diff colours. Tinted backgrounds as well as text, so a whole changed line is visible at a
    /// glance rather than only its first character.
    pub added: Color,
    pub added_bg: Color,
    pub removed: Color,
    pub removed_bg: Color,
    pub font_size: f32,
    pub font_family: String,
    /// Multiplies every gap and padding, so "compact" and "comfortable" are one number.
    pub density: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            bg: Color::from_rgb(24, 26, 27),
            surface: Color::from_rgb(32, 34, 36),
            surface_alt: Color::from_rgb(40, 43, 45),
            border: Color::from_rgb(56, 60, 62),
            text: Color::from_rgb(226, 229, 230),
            text_dim: Color::from_rgb(140, 146, 149),
            accent: Color::from_rgb(45, 212, 191),
            danger: Color::from_rgb(248, 113, 113),
            added: Color::from_rgb(126, 231, 135),
            added_bg: Color::from_rgb(20, 45, 28),
            removed: Color::from_rgb(255, 138, 138),
            removed_bg: Color::from_rgb(58, 24, 26),
            font_size: 14.,
            font_family: "Inter".into(),
            density: 1.,
        }
    }
}

impl Theme {
    pub fn gap(&self, base: f32) -> f32 {
        base * self.density
    }

    /// Project this palette onto Freya's global theme.
    ///
    /// Built-in widgets resolve their colours through named tokens — `MarkdownViewer` reads
    /// `surface_tertiary` for code blocks, `Input` reads `border_focus`, `Table` reads the surface
    /// ramp. Setting the tokens once here themes all of them; overriding each component's theme
    /// individually would mean re-doing it for every widget added later.
    pub fn to_freya(&self) -> FreyaTheme {
        // The code editor's own themes are *not* part of `dark_theme()` and default to light, so
        // without this the editor renders as a white slab in an otherwise dark app. This is the
        // themeable path the standalone `code-editor` provides and `markdown-code-editor` does
        // not — see the note in Cargo.toml.
        let mut t = dark_theme().with_dark_code_editor();
        t.colors.primary = self.accent;
        // `secondary` and `tertiary` are the lighter/darker states (hover, pressed, selected).
        // Leaving them at Freya's defaults is not cosmetic drift — hovering any filled button
        // flashes the stock purple over an otherwise teal UI. Derived the same way Freya derives
        // them from a system accent, so the ramp stays coherent if `accent` is customised.
        t.colors.secondary = Color::lerp(self.accent, Color::WHITE, 0.65);
        t.colors.tertiary = Color::lerp(self.accent, Color::BLACK, 0.23);
        t.colors.background = self.bg;
        t.colors.surface_primary = self.surface_alt;
        t.colors.surface_secondary = self.surface;
        // Code blocks and blockquotes sit *inside* a surface bubble, so they need to read as
        // recessed rather than raised.
        t.colors.surface_tertiary = self.bg;
        t.colors.border = self.border;
        t.colors.border_focus = self.accent;
        t.colors.text_primary = self.text;
        t.colors.text_secondary = self.text_dim;
        t.colors.text_placeholder = self.text_dim;
        t.colors.error = self.danger;
        t
    }
}
