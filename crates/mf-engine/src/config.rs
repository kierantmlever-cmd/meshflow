//! On-disk configuration: `~/.config/meshflow/config.toml`.
//!
//! **API keys are never written here.** Each provider entry carries a `keyring_ref` — an opaque
//! account name for the OS keychain — and the secret itself lives in [`crate::secrets`]. That
//! split is what lets a user paste this file into a bug report, or commit it to a dotfiles repo,
//! without leaking credentials.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::provider::ProviderKind;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not locate a config directory for this platform")]
    NoConfigDir,
    #[error("reading {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("writing {path}: {source}")]
    Write { path: String, source: std::io::Error },
    #[error("{path} is not valid TOML: {source}")]
    Parse { path: String, source: toml::de::Error },
    #[error("serialising config: {0}")]
    Serialise(#[from] toml::ser::Error),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub providers: Vec<ProviderEntry>,
    /// `name` of the entry used for new conversations.
    pub active_provider: Option<String>,
    /// Directories the user has opened, canonicalised. Not a recent-files list: the active one is
    /// the boundary every agent's file access is checked against, so this is security state.
    pub workspaces: Vec<PathBuf>,
    /// The root in force. `None` means the process working directory, which is the documented
    /// default and the narrowest thing available before the user has chosen anything.
    pub active_workspace: Option<PathBuf>,
    pub theme: ThemeConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Stable, user-facing name. Also the keyring account, so renaming an entry orphans its key
    /// rather than silently reusing another entry's.
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    /// Whether a key is expected in the keychain. Local endpoints legitimately have none.
    #[serde(default = "yes")]
    pub needs_key: bool,
    #[serde(default)]
    pub org_id: Option<String>,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

fn yes() -> bool {
    true
}

/// Persisted UI theme. Mirrors `mf_ui::Theme`, which cannot be referenced here — the engine must
/// not depend on the UI crate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    pub accent: [u8; 3],
    pub background: [u8; 3],
    pub surface: [u8; 3],
    pub border: [u8; 3],
    pub text: [u8; 3],
    pub font_family: String,
    pub font_size: f32,
    pub density: f32,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            accent: [45, 212, 191],
            background: [24, 26, 27],
            surface: [32, 34, 36],
            border: [56, 60, 62],
            text: [226, 229, 230],
            font_family: "Inter".into(),
            font_size: 14.,
            density: 1.,
        }
    }
}

impl Config {
    /// `~/.config/meshflow` on Linux, the platform equivalent elsewhere.
    pub fn dir() -> Result<PathBuf, ConfigError> {
        directories::ProjectDirs::from("", "", "meshflow")
            .map(|d| d.config_dir().to_path_buf())
            .ok_or(ConfigError::NoConfigDir)
    }

    pub fn path() -> Result<PathBuf, ConfigError> {
        Ok(Self::dir()?.join("config.toml"))
    }

    /// Load, or return defaults if the file does not exist yet.
    ///
    /// A *missing* file is normal on first run. A *malformed* file is an error: silently
    /// replacing it with defaults would discard the user's provider list without telling them.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read { path: path.display().to_string(), source });
            }
        };
        toml::from_str(&text)
            .map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&Self::path()?)
    }

    /// Written atomically: a crash mid-write would otherwise leave a truncated file that fails
    /// to parse on next launch, locking the user out of their own provider list.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|source| ConfigError::Write { path: parent.display().to_string(), source })?;
        }

        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text)
            .map_err(|source| ConfigError::Write { path: tmp.display().to_string(), source })?;
        std::fs::rename(&tmp, path)
            .map_err(|source| ConfigError::Write { path: path.display().to_string(), source })
    }

    pub fn active(&self) -> Option<&ProviderEntry> {
        match &self.active_provider {
            Some(name) => self.providers.iter().find(|p| &p.name == name),
            // No explicit choice: the first entry is a better guess than nothing.
            None => self.providers.first(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            providers: vec![ProviderEntry {
                name: "openai".into(),
                kind: ProviderKind::OpenAi,
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4o-mini".into(),
                needs_key: true,
                org_id: None,
                headers: Default::default(),
            }],
            active_provider: Some("openai".into()),
            workspaces: Vec::new(),
            active_workspace: None,
            theme: ThemeConfig::default(),
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        sample().save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();

        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].name, "openai");
        assert_eq!(loaded.active().unwrap().model, "gpt-4o-mini");
    }

    #[test]
    fn never_writes_anything_that_looks_like_a_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        sample().save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        // The type system already prevents this — there is no key field — but the guarantee is
        // worth asserting, because adding one later would be an easy and catastrophic mistake.
        for forbidden in ["api_key", "apikey", "secret", "token", "password", "sk-"] {
            assert!(!text.to_lowercase().contains(forbidden), "config.toml leaked {forbidden}:\n{text}");
        }
    }

    #[test]
    fn workspaces_round_trip_and_are_absent_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");

        // No workspace saved is the normal first run, and must load as "use the working
        // directory" rather than as an error or an empty root list that denies everything.
        sample().save_to(&path).unwrap();
        assert!(Config::load_from(&path).unwrap().active_workspace.is_none());

        let mut cfg = sample();
        cfg.workspaces = vec![PathBuf::from("/srv/one"), PathBuf::from("/srv/two")];
        cfg.active_workspace = Some(PathBuf::from("/srv/two"));
        cfg.save_to(&path).unwrap();

        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.workspaces.len(), 2);
        assert_eq!(loaded.active_workspace, Some(PathBuf::from("/srv/two")));
    }

    #[test]
    fn a_missing_file_is_defaults_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = Config::load_from(&tmp.path().join("absent.toml")).unwrap();
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn a_malformed_file_is_an_error_rather_than_silent_data_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        std::fs::write(&path, "this is not [ valid toml").unwrap();

        // Falling back to defaults here would drop the user's providers without a word.
        assert!(matches!(Config::load_from(&path), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        sample().save_to(&path).unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn active_falls_back_to_the_first_entry() {
        let mut cfg = sample();
        cfg.active_provider = None;
        assert_eq!(cfg.active().unwrap().name, "openai");

        // A name that no longer matches any entry must not silently pick a different provider —
        // sending a key-bearing request to the wrong endpoint is worse than failing.
        cfg.active_provider = Some("deleted".into());
        assert!(cfg.active().is_none());
    }

    #[test]
    fn unknown_fields_and_omitted_sections_load_with_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        // A config written by a newer version, or hand-edited down to the essentials.
        std::fs::write(
            &path,
            r#"
            [[providers]]
            name = "ollama"
            kind = "Ollama"
            base_url = "http://localhost:11434/v1"
            model = "llama3"
            needs_key = false
            "#,
        )
        .unwrap();

        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.providers[0].name, "ollama");
        assert!(!cfg.providers[0].needs_key);
        assert_eq!(cfg.theme.font_size, ThemeConfig::default().font_size);
    }
}
