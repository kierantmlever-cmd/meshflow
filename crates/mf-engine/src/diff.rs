//! Unified diffs, for showing the user what a write actually changes.
//!
//! An approval prompt that shows only the *new* contents asks the user to consent to destroying
//! something they cannot see. For a file that already exists, the thing being approved is the
//! change, so that is what gets rendered.

use similar::{ChangeTag, TextDiff};

/// Lines of unchanged context around each change. Three is the `diff -u` convention and enough
/// to recognise where in a file a hunk lands.
const CONTEXT: usize = 3;

/// Cap on rendered lines. A modal the user has to scroll for a minute is one they dismiss
/// unread, which defeats the point of showing it at all. Counts stay exact past the cap.
const MAX_LINES: usize = 400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// Unified-diff text: `+`, `-` and ` ` prefixes, `…` between hunks.
    pub text: String,
    pub added: usize,
    pub removed: usize,
}

impl Diff {
    /// `+12 −3`, for a title or an audit row. Uses a real minus sign so it does not read as a
    /// hyphenated range.
    pub fn tally(&self) -> String {
        format!("+{} −{}", self.added, self.removed)
    }

    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// Diff two texts by line.
pub fn unified(old: &str, new: &str) -> Diff {
    let diff = TextDiff::from_lines(old, new);
    let mut text = String::new();
    let (mut added, mut removed) = (0, 0);
    let mut rendered = 0usize;
    let mut clipped = false;

    for (i, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if i > 0 && rendered < MAX_LINES {
            text.push_str("…\n");
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => {
                        removed += 1;
                        '-'
                    }
                    ChangeTag::Insert => {
                        added += 1;
                        '+'
                    }
                    ChangeTag::Equal => ' ',
                };

                // The tally keeps counting past the cap, so the header stays truthful even when
                // the body is clipped.
                if rendered >= MAX_LINES {
                    clipped = true;
                    continue;
                }
                text.push(sign);
                text.push_str(change.value());
                if !change.value().ends_with('\n') {
                    text.push('\n');
                }
                rendered += 1;
            }
        }
    }

    if clipped {
        text.push_str(&format!("…\n[diff clipped at {MAX_LINES} lines]\n"));
    }
    Diff { text, added, removed }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_changed_line_shows_both_sides_with_context() {
        let d = unified("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!((d.added, d.removed), (1, 1));
        assert!(d.text.contains("-b\n"), "{}", d.text);
        assert!(d.text.contains("+B\n"), "{}", d.text);
        assert!(d.text.contains(" a\n"), "context is what locates the change:\n{}", d.text);
    }

    #[test]
    fn identical_text_produces_nothing() {
        // The caller uses this to say "no change" rather than showing an empty modal.
        let d = unified("same\n", "same\n");
        assert!(d.is_empty());
        assert_eq!(d.text, "");
    }

    #[test]
    fn a_pure_append_removes_nothing() {
        let d = unified("one\n", "one\ntwo\n");
        assert_eq!((d.added, d.removed), (1, 0));
        assert_eq!(d.tally(), "+1 −0");
    }

    #[test]
    fn deleting_everything_is_visible_as_deletion() {
        // The case that most needs showing: an agent replacing a file with an empty one.
        let d = unified("a\nb\nc\n", "");
        assert_eq!(d.removed, 3);
        assert_eq!(d.added, 0);
        assert!(d.text.contains("-a\n") && d.text.contains("-c\n"));
    }

    #[test]
    fn distant_changes_are_separated_rather_than_dumped_whole() {
        let old: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut new_lines: Vec<String> = (0..100).map(|i| format!("line {i}\n")).collect();
        new_lines[2] = "CHANGED\n".into();
        new_lines[90] = "ALSO CHANGED\n".into();
        let new: String = new_lines.concat();

        let d = unified(&old, &new);
        assert_eq!((d.added, d.removed), (2, 2));
        assert!(d.text.contains('…'), "hunks must be elided, not concatenated");
        // Two hunks of ~7 lines, not the whole 100-line file.
        assert!(d.text.lines().count() < 25, "rendered {} lines", d.text.lines().count());
    }

    #[test]
    fn an_enormous_diff_is_clipped_but_the_tally_stays_exact() {
        // A clipped body is fine; a wrong count is not, because that is the number the user
        // actually reads before deciding.
        let new: String = (0..MAX_LINES * 3).map(|i| format!("new line {i}\n")).collect();
        let d = unified("", &new);

        assert_eq!(d.added, MAX_LINES * 3, "the tally must not stop at the render cap");
        assert!(d.text.contains("clipped"));
        assert!(d.text.lines().count() < MAX_LINES + 10);
    }

    #[test]
    fn a_file_with_no_trailing_newline_still_renders_one_line_per_line() {
        let d = unified("a\nb", "a\nc");
        // Without the explicit newline, `-b+c` would run together into one unreadable line.
        assert!(d.text.contains("-b\n"), "{}", d.text);
        assert!(d.text.contains("+c\n"), "{}", d.text);
    }
}
