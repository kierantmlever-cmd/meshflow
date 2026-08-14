//! Literal text search and replace across the workspace.
//!
//! Deliberately plain substring matching, not regex. A coding agent and a person hunting for an
//! identifier both want the literal thing they typed, and a regex engine here would mean a
//! *replace* that can rewrite files in ways the preview never showed. Regex is a Phase 3 concern,
//! alongside the tantivy index that will make this obsolete for large trees.
//!
//! Shared by the UI's search panel and the `search_files` tool, so the agent and the user get the
//! same answers and the same boundary — see [`crate::files`] for why that matters.

use std::path::{Path, PathBuf};

use crate::fsaccess::{Op, PathPolicy};

/// Directories never descended into. Same list as the file tree, and for the same reason: a
/// search that spends its budget inside `target/` finds nothing anyone asked for.
const NOISE: &[&str] = &[".git", "target", "node_modules", ".venv", "__pycache__", ".mypy_cache"];

/// Hits returned before the search stops. Well past what anyone reads, and low enough that a
/// query like `e` cannot marshal a million rows across the channel.
const MAX_HITS: usize = 500;

/// Files opened before the walk gives up.
const MAX_FILES: usize = 5000;

/// Files above this are skipped. Matches the editor's limit — a hit inside a 100 MB file is not
/// something this UI can do anything with.
const MAX_FILE: u64 = 8 * 1024 * 1024;

/// Longest line returned with a hit. A minified bundle is one 2 MB line, and shipping it whole
/// would freeze the row that tries to render it.
const MAX_LINE: usize = 300;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: PathBuf,
    /// 1-based, matching what every editor and compiler prints.
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub files_searched: usize,
    /// True when the search stopped early, so the UI can say "first 500" rather than present a
    /// prefix as if it were the whole answer.
    pub truncated: bool,
}

/// Byte offsets of every non-overlapping occurrence of `needle`.
///
/// Case-insensitive matching folds **ASCII only**, on purpose. Full Unicode folding can change a
/// string's byte length — `İ` lowercases to two chars — which slides every offset after it and
/// makes a replacement land in the wrong place. Getting that wrong corrupts files silently, and
/// ASCII folding is what code identifiers actually need.
fn find_all(haystack: &str, needle: &str, case_sensitive: bool) -> Vec<usize> {
    // An empty needle matches at every position; replacing it would loop forever.
    if needle.is_empty() {
        return Vec::new();
    }

    let (hay, need) = if case_sensitive {
        (haystack.to_owned(), needle.to_owned())
    } else {
        (haystack.to_ascii_lowercase(), needle.to_ascii_lowercase())
    };

    let mut offsets = Vec::new();
    let mut from = 0;
    while let Some(i) = hay[from..].find(&need) {
        let at = from + i;
        offsets.push(at);
        from = at + need.len();
    }
    offsets
}

/// Replace every occurrence, returning the new text and how many were replaced.
fn replace_in(text: &str, needle: &str, replacement: &str, case_sensitive: bool) -> (String, usize) {
    let offsets = find_all(text, needle, case_sensitive);
    if offsets.is_empty() {
        return (text.to_owned(), 0);
    }

    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for at in &offsets {
        out.push_str(&text[last..*at]);
        out.push_str(replacement);
        last = at + needle.len();
    }
    out.push_str(&text[last..]);
    (out, offsets.len())
}

/// Every file under `root` the policy allows, depth first, skipping noise and oversized files.
fn walk(policy: &PathPolicy, root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();

    while let Some(dir) = stack.pop() {
        let Ok(reader) = std::fs::read_dir(&dir) else { continue };
        for item in reader.flatten() {
            let path = item.path();
            let is_dir = item.file_type().map(|t| t.is_dir()).unwrap_or(false);

            if is_dir {
                let name = item.file_name().to_string_lossy().into_owned();
                // Symlinked directories are not followed: a link pointing at its own ancestor
                // makes this walk forever, and one pointing outside would hand the search a tree
                // the policy would refuse file by file anyway.
                if NOISE.contains(&name.as_str()) || item.file_type().is_ok_and(|t| t.is_symlink()) {
                    continue;
                }
                if policy.check(&path, Op::Read).is_ok() {
                    stack.push(path);
                }
                continue;
            }

            if item.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > MAX_FILE {
                continue;
            }
            if policy.check(&path, Op::Read).is_ok() {
                files.push(path);
            }
            if files.len() >= MAX_FILES {
                return files;
            }
        }
    }
    files
}

pub async fn search(
    policy: &PathPolicy,
    root: &Path,
    query: String,
    case_sensitive: bool,
) -> Result<Results, String> {
    if query.is_empty() {
        return Ok(Results::default());
    }
    let root = policy.check(root, Op::Read).map_err(|e| e.to_string())?;
    let policy = policy.clone();

    tokio::task::spawn_blocking(move || {
        let files = walk(&policy, &root);
        let mut results = Results { files_searched: files.len(), ..Default::default() };

        for path in files {
            // A binary file fails here rather than being scanned as mojibake, which is the
            // behaviour we want and costs nothing to get.
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if find_all(&text, &query, case_sensitive).is_empty() {
                continue;
            }

            for (i, line) in text.lines().enumerate() {
                if find_all(line, &query, case_sensitive).is_empty() {
                    continue;
                }
                if results.hits.len() >= MAX_HITS {
                    results.truncated = true;
                    return Ok(results);
                }
                results.hits.push(Hit {
                    path: path.clone(),
                    line: i as u32 + 1,
                    text: line.chars().take(MAX_LINE).collect(),
                });
            }
        }
        Ok(results)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Replace across the workspace. Returns `(files_changed, replacements)`.
///
/// Each file is re-read at replace time rather than reusing the text the search saw, so a file
/// edited in between is rewritten from its current contents instead of being reverted to a stale
/// snapshot. Writes go through the same temp-file-and-rename as the editor, so an interrupted
/// replace cannot leave a half-written file.
pub async fn replace(
    policy: &PathPolicy,
    root: &Path,
    query: String,
    replacement: String,
    case_sensitive: bool,
) -> Result<(usize, usize), String> {
    // Guarded here and not only in the UI: this is the function that rewrites files, so the check
    // belongs where the damage would be done.
    if query.is_empty() {
        return Err("Nothing to replace — the search box is empty.".into());
    }
    let root = policy.check(root, Op::Read).map_err(|e| e.to_string())?;
    let policy = policy.clone();

    tokio::task::spawn_blocking(move || {
        let mut files_changed = 0;
        let mut total = 0;

        for path in walk(&policy, &root) {
            // Re-checked for *write*: the walk only established that the file can be read, and a
            // read-only policy must not be able to rewrite anything through this path.
            let Ok(target) = policy.check(&path, Op::Write) else { continue };
            let Ok(text) = std::fs::read_to_string(&target) else { continue };

            let (new_text, count) = replace_in(&text, &query, &replacement, case_sensitive);
            if count == 0 {
                continue;
            }

            let tmp = target.with_extension("meshflow-tmp");
            std::fs::write(&tmp, new_text.as_bytes())
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &target).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("{}: {e}", target.display())
            })?;

            files_changed += 1;
            total += count;
        }
        Ok((files_changed, total))
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
        fs::write(root.join("src/a.rs"), "let alpha = 1;\nlet beta = alpha + 1;\n").unwrap();
        fs::write(root.join("src/b.rs"), "// ALPHA in a comment\n").unwrap();
        fs::write(root.join("target/gen.rs"), "let alpha = 999;\n").unwrap();
        fs::write(root.join(".env"), "alpha=secret\n").unwrap();
        let policy = PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true);
        (tmp, root, policy)
    }

    #[test]
    fn finds_non_overlapping_occurrences() {
        assert_eq!(find_all("aaaa", "aa", true), vec![0, 2]);
        assert_eq!(find_all("abcabc", "abc", true), vec![0, 3]);
        // An empty needle would otherwise match everywhere and make `replace_in` loop.
        assert_eq!(find_all("abc", "", true), Vec::<usize>::new());
    }

    #[test]
    fn case_insensitive_offsets_survive_non_ascii() {
        // The reason folding is ASCII-only: if lowercasing changed byte lengths, every offset
        // after a non-ASCII character would slide and the replacement would land mid-character.
        let text = "café ALPHA café ALPHA";
        let offsets = find_all(text, "alpha", false);
        assert_eq!(offsets.len(), 2);
        for at in offsets {
            assert_eq!(&text[at..at + 5], "ALPHA", "offset {at} did not land on the match");
        }
    }

    #[test]
    fn replaces_every_occurrence_and_counts_them() {
        let (out, n) = replace_in("a b a b a", "a", "X", true);
        assert_eq!((out.as_str(), n), ("X b X b X", 3));

        let (out, n) = replace_in("Foo foo FOO", "foo", "bar", false);
        assert_eq!((out.as_str(), n), ("bar bar bar", 3));

        // Case-sensitive must not touch the others.
        let (out, n) = replace_in("Foo foo FOO", "foo", "bar", true);
        assert_eq!((out.as_str(), n), ("Foo bar FOO", 1));
    }

    #[tokio::test]
    async fn searches_the_workspace_and_respects_the_boundary() {
        let (_tmp, root, policy) = fixture();
        let found = search(&policy, &root, "alpha".into(), false).await.unwrap();

        let paths: Vec<String> = found
            .hits
            .iter()
            .map(|h| format!("{}:{}", h.path.file_name().unwrap().to_string_lossy(), h.line))
            .collect();

        // `target/` is noise and `.env` is policy-denied; a search that surfaced the contents of
        // a credentials file would be handing it straight to whoever is reading the screen.
        assert!(paths.contains(&"a.rs:1".to_owned()), "missing hit: {paths:?}");
        assert!(paths.contains(&"a.rs:2".to_owned()), "missing hit: {paths:?}");
        assert!(paths.contains(&"b.rs:1".to_owned()), "case-insensitive missed: {paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with("gen.rs")), "searched target/: {paths:?}");
        assert!(!paths.iter().any(|p| p.starts_with(".env")), "searched .env: {paths:?}");
    }

    #[tokio::test]
    async fn case_sensitive_search_excludes_the_other_casing() {
        let (_tmp, root, policy) = fixture();
        let found = search(&policy, &root, "alpha".into(), true).await.unwrap();
        assert!(
            found.hits.iter().all(|h| !h.path.ends_with("b.rs")),
            "matched ALPHA in a case-sensitive search"
        );
    }

    #[tokio::test]
    async fn replace_rewrites_only_allowed_files() {
        let (_tmp, root, policy) = fixture();
        let (files, count) =
            replace(&policy, &root, "alpha".into(), "gamma".into(), true).await.unwrap();

        assert_eq!((files, count), (1, 2), "expected both hits in a.rs and nothing else");
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "let gamma = 1;\nlet beta = gamma + 1;\n"
        );
        // Untouched: denied by the policy, and skipped as noise, respectively.
        assert_eq!(fs::read_to_string(root.join(".env")).unwrap(), "alpha=secret\n");
        assert_eq!(fs::read_to_string(root.join("target/gen.rs")).unwrap(), "let alpha = 999;\n");
    }

    #[tokio::test]
    async fn an_empty_query_never_rewrites_anything() {
        let (_tmp, root, policy) = fixture();
        // An empty needle matching everywhere would otherwise splice the replacement between
        // every character of every file in the workspace.
        assert!(replace(&policy, &root, String::new(), "X".into(), true).await.is_err());
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "let alpha = 1;\nlet beta = alpha + 1;\n"
        );
    }

    #[tokio::test]
    async fn replace_leaves_no_temp_files() {
        let (_tmp, root, policy) = fixture();
        replace(&policy, &root, "alpha".into(), "gamma".into(), true).await.unwrap();

        let leftovers: Vec<String> = fs::read_dir(root.join("src"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("meshflow-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }
}
