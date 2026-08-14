//! Text search and replace across the workspace, literal or by pattern.
//!
//! Both modes run through one regex engine — a literal query is the escaped form of itself. That
//! is fewer moving parts than a hand-rolled substring scan *and* more correct: matches come back
//! as byte ranges into the original text, so case-insensitive matching can fold Unicode without
//! the offsets sliding. (The previous literal scanner folded ASCII only, precisely because
//! lowercasing `İ` changes a string's length and would have landed a replacement mid-character.)
//!
//! **The replacement is always literal**, in both modes. `$1` goes in as the three characters
//! `$1`, never as a capture group.
//!
//! ponytail: no capture expansion — the confirm screen shows the *matched* lines and a count, and
//! a rewrite that transforms them into something the user was never shown is exactly what that
//! screen exists to prevent. Add `$1` when the confirmation can render before/after lines.
//!
//! Shared by the UI's search panel and the `search_files` tool, so the agent and the user get the
//! same answers and the same boundary — see [`crate::files`] for why that matters.

use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};

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

/// What to look for, and how.
///
/// One struct rather than a trail of positional booleans: `search(policy, root, q, true, false)`
/// is a call whose two flags can be swapped without the compiler noticing, and one of them
/// decides whether the text is a pattern or not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub case_sensitive: bool,
    /// Treat [`Self::text`] as a regular expression rather than literal text.
    pub regex: bool,
}

impl Query {
    pub fn literal(text: impl Into<String>, case_sensitive: bool) -> Self {
        Self { text: text.into(), case_sensitive, regex: false }
    }

    /// Compile the query, reporting a bad pattern in the terms the user typed it in.
    ///
    /// A malformed pattern is *user input*, not a bug: it arrives on every third keystroke of a
    /// half-typed `(foo`, so it returns an error to display rather than anything louder.
    fn compile(&self) -> Result<Regex, String> {
        let pattern =
            if self.regex { self.text.clone() } else { regex::escape(&self.text) };
        RegexBuilder::new(&pattern)
            .case_insensitive(!self.case_sensitive)
            .build()
            .map_err(|e| format!("Invalid pattern: {e}"))
    }
}

/// Replace every match, returning the new text and how many were replaced.
///
/// The replacement goes in verbatim — see the module docs for why capture expansion is not
/// offered here.
fn replace_in(re: &Regex, text: &str, replacement: &str) -> (String, usize) {
    let count = re.find_iter(text).count();
    if count == 0 {
        return (text.to_owned(), 0);
    }
    (re.replace_all(text, regex::NoExpand(replacement)).into_owned(), count)
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

/// Every searchable file under `root`, relative to it and sorted.
///
/// The same walk the search uses, so the composer's `@` completion can never offer a file the
/// policy denies — the list is filtered by the boundary rather than against it.
pub async fn paths(policy: &PathPolicy, root: &Path) -> Result<Vec<PathBuf>, String> {
    let root = policy.check(root, Op::Read).map_err(|e| e.to_string())?;
    let policy = policy.clone();

    tokio::task::spawn_blocking(move || {
        let mut files: Vec<PathBuf> = walk(&policy, &root)
            .into_iter()
            .map(|path| path.strip_prefix(&root).unwrap_or(&path).to_path_buf())
            .collect();
        files.sort();
        Ok(files)
    })
    .await
    .map_err(|e| e.to_string())?
}

pub async fn search(policy: &PathPolicy, root: &Path, query: Query) -> Result<Results, String> {
    if query.text.is_empty() {
        return Ok(Results::default());
    }
    let re = query.compile()?;
    let root = policy.check(root, Op::Read).map_err(|e| e.to_string())?;
    let policy = policy.clone();

    tokio::task::spawn_blocking(move || {
        let files = walk(&policy, &root);
        let mut results = Results { files_searched: files.len(), ..Default::default() };

        for path in files {
            // A binary file fails here rather than being scanned as mojibake, which is the
            // behaviour we want and costs nothing to get.
            let Ok(text) = std::fs::read_to_string(&path) else { continue };

            // Matched line by line, with no whole-file pre-check first. A pre-check would be
            // faster but wrong for anchors: `^fn ` matches at the start of *the text*, so a file
            // whose match is on line 40 would be skipped before the loop ever saw it.
            for (i, line) in text.lines().enumerate() {
                if !re.is_match(line) {
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
    query: Query,
    replacement: String,
) -> Result<(usize, usize), String> {
    // Guarded here and not only in the UI: this is the function that rewrites files, so the check
    // belongs where the damage would be done.
    if query.text.is_empty() {
        return Err("Nothing to replace — the search box is empty.".into());
    }
    let re = query.compile()?;
    // `a*`, `^`, `\b` and friends match at every position without consuming anything, so a
    // replace would splice the replacement between every character of every file in the
    // workspace. Refused rather than run: the search panel shows these as one hit per line,
    // which is nothing like what the rewrite would do.
    if re.is_match("") {
        return Err(format!(
            "`{}` matches the empty string, so replacing it would insert `{replacement}` between \
             every character. Narrow the pattern.",
            query.text
        ));
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

            let (new_text, count) = replace_in(&re, &text, &replacement);
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

    fn replace_literal(text: &str, needle: &str, to: &str, case_sensitive: bool) -> (String, usize) {
        replace_in(&Query::literal(needle, case_sensitive).compile().unwrap(), text, to)
    }

    #[test]
    fn replaces_every_occurrence_and_counts_them() {
        let (out, n) = replace_literal("a b a b a", "a", "X", true);
        assert_eq!((out.as_str(), n), ("X b X b X", 3));

        let (out, n) = replace_literal("Foo foo FOO", "foo", "bar", false);
        assert_eq!((out.as_str(), n), ("bar bar bar", 3));

        // Case-sensitive must not touch the others.
        let (out, n) = replace_literal("Foo foo FOO", "foo", "bar", true);
        assert_eq!((out.as_str(), n), ("Foo bar FOO", 1));
    }

    #[test]
    fn a_literal_query_is_never_read_as_a_pattern() {
        // Typing `a.c` into the box means `a.c`, not "a, anything, c".
        let (out, n) = replace_literal("a.c abc", "a.c", "X", true);
        assert_eq!((out.as_str(), n), ("X abc", 1));
    }

    #[test]
    fn the_replacement_is_literal_in_both_modes() {
        // `$1` is three characters, not a capture group — the confirm screen showed the user
        // matched lines and a count, and expansion would rewrite them into something else.
        let re = Query { text: "(a)(b)".into(), case_sensitive: true, regex: true }
            .compile()
            .unwrap();
        let (out, n) = replace_in(&re, "ab ab", "$1-$2");
        assert_eq!((out.as_str(), n), ("$1-$2 $1-$2", 2));
    }

    #[test]
    fn case_insensitive_matches_survive_non_ascii() {
        // Ranges come from the original text, so folding can never slide an offset into the
        // middle of a multi-byte character.
        let re = Query::literal("alpha", false).compile().unwrap();
        let text = "café ALPHA café ALPHA";
        let spans: Vec<_> = re.find_iter(text).map(|m| m.range()).collect();
        assert_eq!(spans.len(), 2);
        for span in spans {
            assert_eq!(&text[span], "ALPHA");
        }
    }

    #[test]
    fn a_malformed_pattern_is_an_error_not_a_panic() {
        let bad = Query { text: "(unclosed".into(), case_sensitive: true, regex: true };
        assert!(bad.compile().unwrap_err().starts_with("Invalid pattern"));
        // The same characters are perfectly good literal text.
        assert!(Query::literal("(unclosed", true).compile().is_ok());
    }

    #[tokio::test]
    async fn searches_the_workspace_and_respects_the_boundary() {
        let (_tmp, root, policy) = fixture();
        let found = search(&policy, &root, Query::literal("alpha", false)).await.unwrap();

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
        let found = search(&policy, &root, Query::literal("alpha", true)).await.unwrap();
        assert!(
            found.hits.iter().all(|h| !h.path.ends_with("b.rs")),
            "matched ALPHA in a case-sensitive search"
        );
    }

    #[tokio::test]
    async fn replace_rewrites_only_allowed_files() {
        let (_tmp, root, policy) = fixture();
        let (files, count) =
            replace(&policy, &root, Query::literal("alpha", true), "gamma".into()).await.unwrap();

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
        assert!(replace(&policy, &root, Query::default(), "X".into()).await.is_err());
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "let alpha = 1;\nlet beta = alpha + 1;\n"
        );
    }

    #[tokio::test]
    async fn a_pattern_matches_by_line_wherever_the_line_sits_in_the_file() {
        let (_tmp, root, policy) = fixture();
        // `beta` is on line 2. An anchored pattern checked against the whole file first would
        // never reach it.
        let query = Query { text: r"^let \w+".into(), case_sensitive: true, regex: true };
        let found = search(&policy, &root, query).await.unwrap();

        let lines: Vec<u32> =
            found.hits.iter().filter(|h| h.path.ends_with("a.rs")).map(|h| h.line).collect();
        assert_eq!(lines, vec![1, 2], "anchored pattern missed a later line: {found:?}");
    }

    #[tokio::test]
    async fn a_pattern_matching_the_empty_string_is_refused_rather_than_run() {
        let (_tmp, root, policy) = fixture();
        let query = Query { text: "x*".into(), case_sensitive: true, regex: true };

        let err = replace(&policy, &root, query, "X".into()).await.unwrap_err();
        assert!(err.contains("empty string"), "{err}");
        // The workspace is untouched — this is the check that stands between `x*` and every file
        // in the tree being shredded.
        assert_eq!(
            fs::read_to_string(root.join("src/a.rs")).unwrap(),
            "let alpha = 1;\nlet beta = alpha + 1;\n"
        );
    }

    #[tokio::test]
    async fn replace_leaves_no_temp_files() {
        let (_tmp, root, policy) = fixture();
        replace(&policy, &root, Query::literal("alpha", true), "gamma".into()).await.unwrap();

        let leftovers: Vec<String> = fs::read_dir(root.join("src"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("meshflow-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }
}
