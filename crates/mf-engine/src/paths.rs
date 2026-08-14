//! Where config, the database and the logs live.
//!
//! Normally the OS's own locations — `~/.config/meshflow` and `~/.local/share/meshflow` on Linux,
//! `%APPDATA%` and `%LOCALAPPDATA%` on Windows. That is right for an installed application, and
//! wrong for one carried on a USB stick: a "portable" build that scatters state into the profile
//! of every machine it touches is not portable, it is just an executable that leaves a trail.
//!
//! So there are two overrides, checked in order:
//!
//! 1. `MESHFLOW_HOME` — everything under that directory. Explicit, scriptable, and what a
//!    packaged build sets if it wants to decide for itself.
//! 2. A `meshflow-data` directory sitting **next to the executable**. Its presence is the whole
//!    switch: ship the zip with an empty one and the app is portable, delete it and the same
//!    binary goes back to using the OS locations.
//!
//! Deliberately not a config file setting — the file would have to live somewhere to say where
//! the files live.

use std::path::{Path, PathBuf};

/// The directory whose presence beside the executable turns portable mode on.
pub const PORTABLE_DIR: &str = "meshflow-data";

/// The portable root, given the two inputs that decide it. Pure, so the rule is testable without
/// setting environment variables in a process that has other tests running in it.
fn resolve(home_var: Option<&Path>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(home) = home_var {
        return Some(home.to_path_buf());
    }
    // Only when it already exists: creating it on demand would silently convert an installed
    // copy into a portable one the first time it ran.
    let beside = exe_dir?.join(PORTABLE_DIR);
    beside.is_dir().then_some(beside)
}

/// The portable root in force, if any.
pub fn portable_root() -> Option<PathBuf> {
    let home = std::env::var_os("MESHFLOW_HOME").map(PathBuf::from);
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    resolve(home.as_deref(), exe_dir)
}

/// Where `config.toml` lives.
pub fn config_dir() -> Option<PathBuf> {
    portable_root()
        .or_else(|| directories::ProjectDirs::from("", "", "meshflow").map(|d| d.config_dir().to_path_buf()))
}

/// Where the database and the log directory live.
pub fn data_dir() -> Option<PathBuf> {
    portable_root()
        .or_else(|| directories::ProjectDirs::from("", "", "meshflow").map(|d| d.data_dir().to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_var_wins_outright() {
        let root = resolve(Some(Path::new("/opt/mf")), Some(Path::new("/usr/bin")));
        assert_eq!(root, Some(PathBuf::from("/opt/mf")));
    }

    #[test]
    fn a_marker_directory_beside_the_executable_turns_it_on() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path();
        assert_eq!(resolve(None, Some(exe_dir)), None, "absent marker means installed mode");

        std::fs::create_dir(exe_dir.join(PORTABLE_DIR)).unwrap();
        assert_eq!(resolve(None, Some(exe_dir)), Some(exe_dir.join(PORTABLE_DIR)));
    }

    #[test]
    fn a_file_of_that_name_is_not_a_portable_root() {
        // Otherwise a stray `meshflow-data` file would send the database to a path that cannot
        // hold it, and the app would fail to start with a confusing IO error.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(PORTABLE_DIR), "not a directory").unwrap();
        assert_eq!(resolve(None, Some(tmp.path())), None);
    }

    #[test]
    fn nothing_to_go_on_means_the_os_locations() {
        assert_eq!(resolve(None, None), None);
    }
}
