//! Filesystem access for the user's file tree and editor.
//!
//! This is the *user's* surface, not an agent's, so there is no approval gate — clicking a file
//! is the consent, and pressing save is the consent to write. It still goes through the same
//! [`PathPolicy`] as the agent tools, for three reasons:
//!
//! 1. Open editor tabs are slated to become model context (pinned files, `@`-mentions). A file
//!    that must never reach a provider must therefore never become a tab, and reusing the one
//!    deny list is what makes that automatic instead of a rule someone has to remember.
//! 2. One trust boundary beats two that have to be kept in agreement.
//! 3. It runs on the blocking pool, so a directory on a cold spinning disk cannot stall the UI.
//!
//! Nothing is taken away from the user by this: they can still open any file on their machine in
//! any other editor. It only bounds what *this* app will load into a buffer.

use std::path::{Path, PathBuf};

use crate::fsaccess::{Op, PathPolicy};

/// Directories skipped in listings.
///
/// Purely about noise and size — `target/` alone is tens of thousands of files and would make the
/// tree useless and slow. Not a security measure; the policy is what enforces access.
const NOISE: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__", ".mypy_cache"];

/// Entries returned for a single directory before the listing is cut short.
///
/// A directory with a hundred thousand files would otherwise be marshalled across the channel and
/// laid out by the UI one row at a time, which reads to the user as a freeze.
const MAX_ENTRIES: usize = 1000;

/// Largest file loaded into an editor buffer.
///
/// The editor holds a rope and re-parses for highlighting; a multi-hundred-megabyte log opened by
/// a misclick would hang the app long enough to look like a crash. Refusing with a reason is the
/// honest outcome.
const MAX_FILE: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// List `path`, dropping anything the policy would refuse to open.
///
/// Filtering here rather than in the UI keeps the tree honest: an entry that is shown but cannot
/// be opened is a row that does nothing when clicked, which reads as a broken app rather than as
/// a policy doing its job.
pub async fn list(policy: &PathPolicy, path: &Path) -> Result<(Vec<Entry>, bool), String> {
    let dir = policy.check(path, Op::Read).map_err(|e| e.to_string())?;
    let policy = policy.clone();

    tokio::task::spawn_blocking(move || {
        let reader = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

        let mut entries = Vec::new();
        let mut truncated = false;
        for item in reader {
            let Ok(item) = item else { continue };
            let name = item.file_name().to_string_lossy().into_owned();

            // `file_type` does not follow symlinks, which is what we want for the *shape* of the
            // tree; the policy still resolves them before anything is opened.
            let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && NOISE.contains(&name.as_str()) {
                continue;
            }

            let path = item.path();
            if policy.check(&path, Op::Read).is_err() {
                continue;
            }

            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            entries.push(Entry { name, path, is_dir });
        }

        // Directories first, then case-insensitive by name — the ordering every file tree uses,
        // and `read_dir` returns whatever order the filesystem feels like.
        entries.sort_by(|a, b| {
            b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok((entries, truncated))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Read a file into an editor buffer.
pub async fn read(policy: &PathPolicy, path: &Path) -> Result<String, String> {
    let file = policy.check(path, Op::Read).map_err(|e| e.to_string())?;

    tokio::task::spawn_blocking(move || {
        let size = std::fs::metadata(&file).map_err(|e| format!("{}: {e}", file.display()))?.len();
        if size > MAX_FILE {
            return Err(format!(
                "{} is {:.1} MB. Files over {} MB are not opened here — the editor would stall \
                 long enough to look like a crash.",
                file.display(),
                size as f64 / 1_048_576.,
                MAX_FILE / 1_048_576,
            ));
        }

        // Refused rather than lossily converted: opening a binary as replacement characters and
        // then saving would silently destroy it.
        std::fs::read_to_string(&file).map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                format!("{} is not UTF-8 text, so it cannot be edited here.", file.display())
            } else {
                format!("{}: {e}", file.display())
            }
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Save an editor buffer.
///
/// Written to a temporary file and renamed, so an interrupted save leaves the previous version
/// intact rather than a half-written file. Losing the edit is recoverable; losing the original is
/// not.
pub async fn write(policy: &PathPolicy, path: &Path, content: String) -> Result<(), String> {
    let file = policy.check(path, Op::Write).map_err(|e| e.to_string())?;

    tokio::task::spawn_blocking(move || {
        let tmp = file.with_extension("meshflow-tmp");
        std::fs::write(&tmp, content.as_bytes())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &file).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", file.display())
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::fsaccess::AccessMode;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathPolicy) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("README.md"), "# hi").unwrap();
        fs::write(root.join(".env"), "SECRET=1").unwrap();
        let policy = PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true);
        (tmp, root, policy)
    }

    #[tokio::test]
    async fn lists_directories_first_and_hides_denied_files() {
        let (_tmp, root, policy) = fixture();
        let (entries, truncated) = list(&policy, &root).await.unwrap();

        assert!(!truncated);
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        // `target` is noise, `.env` is denied by the policy. Showing either would give the user a
        // row that does nothing when clicked.
        assert_eq!(names, vec!["src", "README.md"], "unexpected listing: {names:?}");
        assert!(entries[0].is_dir);
    }

    #[tokio::test]
    async fn refuses_to_list_outside_the_sandbox() {
        let (tmp, _root, policy) = fixture();
        assert!(list(&policy, tmp.path()).await.is_err());
    }

    #[tokio::test]
    async fn reads_and_saves_a_file() {
        let (_tmp, root, policy) = fixture();
        let file = root.join("src/main.rs");

        assert_eq!(read(&policy, &file).await.unwrap(), "fn main() {}");
        write(&policy, &file, "fn main() { println!(); }".into()).await.unwrap();
        assert_eq!(read(&policy, &file).await.unwrap(), "fn main() { println!(); }");
    }

    #[tokio::test]
    async fn a_save_leaves_no_temp_file_behind() {
        let (_tmp, root, policy) = fixture();
        write(&policy, &root.join("README.md"), "# changed".into()).await.unwrap();

        let leftovers: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("meshflow-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn a_denied_file_cannot_be_opened_by_naming_it_directly() {
        let (_tmp, root, policy) = fixture();
        // Hiding `.env` from the listing is presentation. This is the part that matters: naming
        // it explicitly must still fail, because the tree is not the thing enforcing anything.
        assert!(read(&policy, &root.join(".env")).await.is_err());
        assert!(write(&policy, &root.join(".env"), "SECRET=2".into()).await.is_err());
    }

    #[tokio::test]
    async fn binary_files_are_refused_rather_than_mangled() {
        let (_tmp, root, policy) = fixture();
        let blob = root.join("blob.bin");
        fs::write(&blob, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        // Lossy conversion here would show replacement characters and then *save* them over the
        // original on the first Ctrl+S.
        let err = read(&policy, &blob).await.unwrap_err();
        assert!(err.contains("not UTF-8"), "unexpected error: {err}");
    }
}
