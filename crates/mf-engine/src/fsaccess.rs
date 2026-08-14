//! The filesystem trust boundary.
//!
//! Every path an agent supplies passes through [`PathPolicy::check`] before it is opened. A bug
//! here defeats the entire security model, so the rules are deliberately blunt:
//!
//! 1. **Resolve, then authorise.** Never the other way around. A string-prefix test on an
//!    unresolved path loses to `..`, to symlinks, and to `/proc/self/root`.
//! 2. **Deny-by-default globs apply in every mode**, including `FullSystem`.
//! 3. **A denial says why**, so the model corrects itself instead of retrying blindly.

use std::path::{Component, Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AccessMode {
    /// Only the process's working directory. The default, and the only safe one to start in.
    #[default]
    Cwd,
    /// The workspace root and nothing above it.
    WorkspaceSandbox,
    /// Anywhere the deny list permits. Still requires approval for every write.
    FullSystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Denied {
    #[error("path is outside the allowed roots ({mode:?}): {path}")]
    OutsideRoots { path: String, mode: AccessMode },
    #[error("path matches a always-denied pattern (secrets, VCS metadata): {path}")]
    DeniedPattern { path: String },
    #[error("writes are not permitted for this agent: {path}")]
    ReadOnly { path: String },
    #[error("path does not resolve: {path} ({reason})")]
    Unresolvable { path: String, reason: String },
}

/// Patterns denied in every mode, including `FullSystem`.
///
/// These are the things a coding agent has no legitimate reason to read and that cause real harm
/// when they reach a model provider: credentials, keys, and this app's own secret storage.
const ALWAYS_DENY: &[&str] = &[
    "**/.git/**",
    "**/.env",
    "**/.env.*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/.ssh/**",
    "**/.aws/**",
    "**/.gnupg/**",
    "**/.netrc",
    "**/.npmrc",
    "**/.pypirc",
    "**/meshflow/config.toml",
    "**/meshflow/meshflow.db*",
];

#[derive(Debug, Clone)]
pub struct PathPolicy {
    mode: AccessMode,
    roots: Vec<PathBuf>,
    deny: GlobSet,
    allow_write: bool,
}

impl PathPolicy {
    /// `roots` are canonicalised up front; any that don't resolve are dropped rather than
    /// silently widening access.
    pub fn new(mode: AccessMode, roots: impl IntoIterator<Item = PathBuf>, allow_write: bool) -> Self {
        let mut builder = GlobSetBuilder::new();
        for pattern in ALWAYS_DENY {
            builder.add(Glob::new(pattern).expect("ALWAYS_DENY patterns are valid"));
        }
        Self {
            mode,
            roots: roots.into_iter().filter_map(|r| r.canonicalize().ok()).collect(),
            deny: builder.build().expect("globset builds"),
            allow_write,
        }
    }

    /// The policy an agent starts with: the working directory, read and write.
    pub fn cwd(allow_write: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::new(AccessMode::Cwd, [cwd], allow_write)
    }

    pub fn mode(&self) -> AccessMode {
        self.mode
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Resolve `path` and authorise `op` against it, returning the canonical path to use.
    ///
    /// The returned path is the one the caller must open — using the original would reintroduce
    /// the TOCTOU gap this function exists to close.
    pub fn check(&self, path: &Path, op: Op) -> Result<PathBuf, Denied> {
        if op == Op::Write && !self.allow_write {
            return Err(Denied::ReadOnly { path: path.display().to_string() });
        }

        let resolved = self.resolve(path)?;

        // Checked on the *resolved* path: `foo/../.ssh/id_rsa` must not slip past a pattern match.
        if self.deny.is_match(&resolved) {
            return Err(Denied::DeniedPattern { path: resolved.display().to_string() });
        }

        if self.mode != AccessMode::FullSystem && !self.within_roots(&resolved) {
            return Err(Denied::OutsideRoots {
                path: resolved.display().to_string(),
                mode: self.mode,
            });
        }

        Ok(resolved)
    }

    /// Canonicalise, tolerating a final component that does not exist yet.
    ///
    /// `canonicalize` fails on a path whose leaf is missing, which is exactly the case for
    /// creating a file. Resolving the *parent* and rejoining the leaf keeps symlink resolution
    /// intact — the parent is where a symlink escape would live.
    fn resolve(&self, path: &Path) -> Result<PathBuf, Denied> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.roots
                .first()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(path)
        };

        if let Ok(resolved) = absolute.canonicalize() {
            return Ok(resolved);
        }

        let parent = absolute.parent().ok_or_else(|| Denied::Unresolvable {
            path: path.display().to_string(),
            reason: "no parent directory".into(),
        })?;
        let leaf = absolute.file_name().ok_or_else(|| Denied::Unresolvable {
            path: path.display().to_string(),
            reason: "no file name".into(),
        })?;

        // A `..` leaf has no name to rejoin and would escape the resolved parent.
        if matches!(absolute.components().next_back(), Some(Component::ParentDir)) {
            return Err(Denied::Unresolvable {
                path: path.display().to_string(),
                reason: "traversal in final component".into(),
            });
        }

        let parent = parent.canonicalize().map_err(|e| Denied::Unresolvable {
            path: path.display().to_string(),
            reason: format!("parent directory: {e}"),
        })?;

        Ok(parent.join(leaf))
    }

    fn within_roots(&self, resolved: &Path) -> bool {
        // Component-wise, not string prefix: `/home/user/proj-secrets` must not pass as being
        // inside `/home/user/proj`.
        self.roots.iter().any(|root| resolved.starts_with(root))
    }

    /// A description of the live policy for the system prompt, so the model knows its limits
    /// rather than discovering them through failures.
    pub fn describe(&self) -> String {
        let roots = self
            .roots
            .iter()
            .map(|r| r.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let access = match self.mode {
            AccessMode::Cwd => format!("You may only access files under: {roots}"),
            AccessMode::WorkspaceSandbox => format!("You are sandboxed to the workspace: {roots}"),
            AccessMode::FullSystem => "You may access the whole filesystem.".into(),
        };
        let writes = if self.allow_write { "read and write" } else { "read only" };
        format!(
            "{access}\nFile access is {writes}. Credentials, SSH keys, .env files and VCS \
             metadata are always denied, in every mode."
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A sandbox root with a `secret.txt` sitting *outside* it — the thing every test tries to reach.
    fn fixture() -> (tempfile::TempDir, PathBuf, PathPolicy) {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        let root = tmp.path().join("root");
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(outside.join("secret.txt"), "sensitive").unwrap();
        fs::write(root.join("ok.txt"), "fine").unwrap();

        let policy = PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true);
        (tmp, root, policy)
    }

    #[test]
    fn allows_paths_inside_the_root() {
        let (_tmp, root, policy) = fixture();
        assert!(policy.check(&root.join("ok.txt"), Op::Read).is_ok());
        assert!(policy.check(&root.join("sub"), Op::Read).is_ok());
    }

    #[test]
    fn rejects_parent_traversal() {
        let (_tmp, root, policy) = fixture();
        let escape = root.join("../outside/secret.txt");
        assert!(
            matches!(policy.check(&escape, Op::Read), Err(Denied::OutsideRoots { .. })),
            "`..` must not escape the root"
        );
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let (tmp, _root, policy) = fixture();
        let outside = tmp.path().join("outside/secret.txt");
        assert!(matches!(policy.check(&outside, Op::Read), Err(Denied::OutsideRoots { .. })));
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlink_pointing_outside_root() {
        let (tmp, root, policy) = fixture();
        let link = root.join("escape-link");
        std::os::unix::fs::symlink(tmp.path().join("outside"), &link).unwrap();

        // The link itself lives inside the root; only resolution reveals the escape. This is the
        // case a string-prefix check gets wrong.
        let through_link = link.join("secret.txt");
        assert!(
            matches!(policy.check(&through_link, Op::Read), Err(Denied::OutsideRoots { .. })),
            "symlinks must be resolved before authorising"
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_write_through_symlinked_parent() {
        let (tmp, root, policy) = fixture();
        let link = root.join("out-link");
        std::os::unix::fs::symlink(tmp.path().join("outside"), &link).unwrap();

        // New file, so resolution goes through the parent path — the parent is the symlink.
        let new_file = link.join("planted.txt");
        assert!(
            matches!(policy.check(&new_file, Op::Write), Err(Denied::OutsideRoots { .. })),
            "creating a file through a symlinked parent must be denied"
        );
    }

    #[test]
    fn allows_creating_a_file_that_does_not_exist_yet() {
        let (_tmp, root, policy) = fixture();
        let new_file = root.join("sub/new.txt");
        let resolved = policy.check(&new_file, Op::Write).expect("writing a new file is allowed");
        assert!(resolved.ends_with("sub/new.txt"));
        assert!(resolved.is_absolute(), "callers must open the resolved path");
    }

    #[test]
    fn traversal_in_the_final_component_resolves_rather_than_escaping() {
        let (_tmp, root, policy) = fixture();

        // `root/sub/..` *is* `root`, and `root` is allowed — resolving first is what makes this
        // safe to permit. The check that matters is that it lands on the root and not above it.
        let resolved = policy.check(&root.join("sub/.."), Op::Read).expect("resolves inside root");
        assert_eq!(resolved, root.canonicalize().unwrap());

        // One level higher escapes, and must not.
        assert!(matches!(
            policy.check(&root.join("sub/../.."), Op::Read),
            Err(Denied::OutsideRoots { .. })
        ));
    }

    #[test]
    fn denies_secret_patterns_even_inside_the_root() {
        let (_tmp, root, policy) = fixture();
        fs::create_dir_all(root.join(".ssh")).unwrap();
        fs::write(root.join(".ssh/id_rsa"), "key").unwrap();
        fs::write(root.join(".env"), "TOKEN=1").unwrap();

        for path in [root.join(".ssh/id_rsa"), root.join(".env")] {
            assert!(
                matches!(policy.check(&path, Op::Read), Err(Denied::DeniedPattern { .. })),
                "{} must be denied by pattern",
                path.display()
            );
        }
    }

    #[test]
    fn deny_patterns_apply_in_full_system_mode() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".env"), "TOKEN=1").unwrap();
        let policy = PathPolicy::new(AccessMode::FullSystem, [], true);

        assert!(
            matches!(policy.check(&tmp.path().join(".env"), Op::Read), Err(Denied::DeniedPattern { .. })),
            "FullSystem still refuses credentials"
        );
    }

    #[test]
    fn deny_patterns_are_matched_after_resolution() {
        let (_tmp, root, policy) = fixture();
        fs::create_dir_all(root.join(".ssh")).unwrap();
        fs::write(root.join(".ssh/id_rsa"), "key").unwrap();

        // Spelled so that a naive pattern match on the raw string misses `**/.ssh/**`.
        let sneaky = root.join("sub/../.ssh/id_rsa");
        assert!(
            matches!(policy.check(&sneaky, Op::Read), Err(Denied::DeniedPattern { .. })),
            "patterns must be checked against the resolved path"
        );
    }

    #[test]
    fn read_only_policy_refuses_writes_but_allows_reads() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "x").unwrap();
        let policy = PathPolicy::new(AccessMode::WorkspaceSandbox, [tmp.path().to_path_buf()], false);

        assert!(policy.check(&tmp.path().join("a.txt"), Op::Read).is_ok());
        assert!(matches!(
            policy.check(&tmp.path().join("a.txt"), Op::Write),
            Err(Denied::ReadOnly { .. })
        ));
    }

    #[test]
    fn sibling_directory_with_shared_prefix_is_not_inside_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        let sibling = tmp.path().join("proj-secrets");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("creds.txt"), "sensitive").unwrap();

        let policy = PathPolicy::new(AccessMode::WorkspaceSandbox, [root], true);
        assert!(
            matches!(policy.check(&sibling.join("creds.txt"), Op::Read), Err(Denied::OutsideRoots { .. })),
            "`/proj-secrets` shares a string prefix with `/proj` but is not inside it"
        );
    }

    #[test]
    fn describe_names_the_roots_so_the_model_can_self_correct() {
        let (_tmp, root, policy) = fixture();
        let described = policy.describe();
        assert!(described.contains(&root.canonicalize().unwrap().display().to_string()));
        assert!(described.contains("read and write"));
    }
}
