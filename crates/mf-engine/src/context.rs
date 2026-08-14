//! Fitting a conversation into the model's context window.
//!
//! The history a run sends grows with every tool result — up to 64 KB each, up to 25 round-trips
//! per turn, and it never shrinks between turns. Without a budget the request eventually exceeds
//! the window and the provider rejects the *whole* turn, which reads to the user as the app
//! breaking rather than the conversation being too long.
//!
//! Trimming happens at **user-turn boundaries**, never inside one. A tool result whose matching
//! tool call has been dropped is a malformed request that every provider rejects, so a smarter
//! trim that shaved individual messages would trade a recoverable failure for an unrecoverable
//! one. Whole turns go, oldest first.

use std::path::Path;

use crate::{
    files,
    fsaccess::PathPolicy,
    provider::{Message, Part, Role},
};

/// Bytes per token. English prose runs ~4, code and JSON a little denser.
///
/// ponytail: a heuristic, not a tokenizer — the real count comes back in `Usage` after the fact,
/// and a proper BPE tokenizer is a per-provider dependency (`tiktoken`, and no offline equivalent
/// for Anthropic) that would have to be right for every model to be worth carrying. Reserve
/// enough headroom that being 25% wrong still fits, and revisit if a turn is ever rejected for
/// length.
const BYTES_PER_TOKEN: usize = 4;

/// Per-message framing: role markers, delimiters, the tool-call envelope. Small, but a hundred
/// short messages make it real.
const MESSAGE_OVERHEAD: usize = 4;

/// Estimated input tokens for one message.
pub fn cost(message: &Message) -> usize {
    let bytes: usize = message
        .content
        .iter()
        .map(|part| match part {
            Part::Text(text) => text.len(),
            // The arguments go on the wire as JSON, so that is what they cost.
            Part::ToolCall { name, args, .. } => name.len() + args.to_string().len(),
            Part::ToolResult { content, .. } => content.len(),
        })
        .sum();
    bytes / BYTES_PER_TOKEN + MESSAGE_OVERHEAD
}

/// The index to start sending history from, so that `messages[start..]` fits in `budget` tokens.
///
/// Returns `0` when everything fits, which is the common case and costs one pass.
///
/// The newest user turn is always kept, even when it alone exceeds the budget: there is nothing
/// valid left to drop at that point, and sending it lets the provider's own error say so.
pub fn fit(messages: &[Message], budget: usize) -> usize {
    let mut used = 0;
    let mut keep_from = None;
    let mut newest_user = None;

    // Backwards, because the newest turn is the one that must survive. `used` only grows, so the
    // first user boundary that does not fit rules out every older one.
    for (i, message) in messages.iter().enumerate().rev() {
        used += cost(message);
        if message.role == Role::User {
            newest_user.get_or_insert(i);
            if used <= budget {
                keep_from = Some(i);
            } else {
                break;
            }
        }
    }

    keep_from.or(newest_user).unwrap_or(0)
}

/// Files one message may pull in. A paragraph that mentions twenty paths is a paste, not a
/// request, and attaching all of them would spend the window before the model saw the question.
const MAX_MENTIONS: usize = 10;

/// Trailing characters stripped from a mention. `@src/main.rs.` ends a sentence; `@(foo)` is a
/// path inside brackets. Only the *end* is trimmed, so `main.rs` keeps its extension.
const TRAILING: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\''];

/// Paths named with `@` in a message, in the order written, without repeats.
///
/// A `@` must open a word: `user@host` and `a@b.com` are addresses, not attachments.
pub fn mentions(text: &str) -> Vec<&str> {
    let mut found: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();

    for (i, _) in text.match_indices('@') {
        if i > 0 && !bytes[i - 1].is_ascii_whitespace() {
            continue;
        }
        let rest = &text[i + 1..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let path = rest[..end].trim_end_matches(TRAILING);
        if !path.is_empty() && !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

/// Read every `@`-mentioned file into one block to put in front of the user's message.
///
/// Returns `None` when nothing was mentioned, which is the overwhelmingly common case.
///
/// Reads go through the same [`PathPolicy`] as the agent's tools and the editor, so a file the
/// workspace denies — a `.env`, an SSH key — cannot be handed to a provider by typing its name.
/// A failure is reported *in the block* rather than dropped: the model needs to know the file it
/// was told about is missing, or it will answer as if it had read it.
pub async fn attach(policy: &PathPolicy, root: &Path, text: &str) -> Option<String> {
    let mentions = mentions(text);
    if mentions.is_empty() {
        return None;
    }

    let mut block = String::from("Files the user attached to this message:\n\n");
    for name in mentions.iter().take(MAX_MENTIONS) {
        let path = Path::new(name);
        // Mentions are relative to the workspace, not to wherever the process was started.
        let full = if path.is_absolute() { path.to_path_buf() } else { root.join(path) };

        let body = match files::read(policy, &full).await {
            Ok(text) => crate::tool::builtins::truncate(text, "file"),
            Err(why) => {
                tracing::warn!(mention = name, %why, "could not attach a mentioned file");
                format!("(could not be attached: {why})")
            }
        };
        block.push_str(&format!("<file path=\"{name}\">\n{body}\n</file>\n\n"));
    }

    if mentions.len() > MAX_MENTIONS {
        block.push_str(&format!(
            "({} further mentions were not attached — only the first {MAX_MENTIONS} are.)\n\n",
            mentions.len() - MAX_MENTIONS,
        ));
    }
    Some(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsaccess::AccessMode;
    use serde_json::json;

    /// A user message, the assistant's tool call, and its result — one indivisible turn.
    fn turn(id: &str, size: usize) -> Vec<Message> {
        vec![
            Message::user("do the thing"),
            Message {
                role: Role::Assistant,
                content: vec![Part::ToolCall {
                    id: id.into(),
                    name: "read_file".into(),
                    args: json!({ "path": "a.txt" }),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![Part::ToolResult {
                    id: id.into(),
                    content: "x".repeat(size),
                    is_error: false,
                }],
            },
        ]
    }

    /// Every tool result in the kept slice must have its call, and the slice must open on a user
    /// message — the two things a provider rejects the request for.
    fn assert_well_formed(kept: &[Message]) {
        assert_eq!(kept.first().map(|m| m.role), Some(Role::User), "history must open on a user turn");

        let calls: Vec<&str> = kept
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|p| match p {
                Part::ToolCall { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        for part in kept.iter().flat_map(|m| &m.content) {
            if let Part::ToolResult { id, .. } = part {
                assert!(calls.contains(&id.as_str()), "orphaned tool result {id}");
            }
        }
    }

    #[test]
    fn reads_mentions_as_written() {
        assert_eq!(mentions("look at @src/a.rs and @b.rs, please"), vec!["src/a.rs", "b.rs"]);
        // Sentence punctuation is not part of the path.
        assert_eq!(mentions("see @src/main.rs."), vec!["src/main.rs"]);
        // An address is not an attachment.
        assert_eq!(mentions("mail user@host.com"), Vec::<&str>::new());
        // The same file twice is one attachment.
        assert_eq!(mentions("@a.rs then @a.rs again"), vec!["a.rs"]);
        assert_eq!(mentions("no mentions here"), Vec::<&str>::new());
        assert_eq!(mentions("a bare @ is not a path"), Vec::<&str>::new());
    }

    fn workspace() -> (tempfile::TempDir, std::path::PathBuf, PathPolicy) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join(".env"), "API_KEY=sk-live-secret\n").unwrap();
        let root = root.canonicalize().unwrap();
        let policy = PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true);
        (tmp, root, policy)
    }

    #[tokio::test]
    async fn attaches_a_mentioned_file_and_nothing_else() {
        let (_tmp, root, policy) = workspace();
        let block = attach(&policy, &root, "explain @src/a.rs").await.unwrap();

        assert!(block.contains("fn main() {}"), "{block}");
        assert!(block.contains(r#"<file path="src/a.rs">"#), "{block}");
        assert!(attach(&policy, &root, "no mentions").await.is_none());
    }

    #[tokio::test]
    async fn a_denied_file_cannot_be_attached_by_naming_it() {
        let (_tmp, root, policy) = workspace();
        // The whole point of routing attachments through the policy: typing the name of a
        // credentials file must not be a way to put it in a provider request.
        let block = attach(&policy, &root, "check @.env").await.unwrap();

        assert!(!block.contains("sk-live-secret"), "a denied file was attached: {block}");
        assert!(block.contains("could not be attached"), "{block}");
    }

    #[tokio::test]
    async fn a_missing_file_is_reported_rather_than_dropped() {
        let (_tmp, root, policy) = workspace();
        // Silence would leave the model answering about a file it never received.
        let block = attach(&policy, &root, "look at @src/nope.rs").await.unwrap();
        assert!(block.contains("could not be attached"), "{block}");
    }

    #[test]
    fn keeps_everything_that_fits() {
        let history = turn("a", 100);
        assert_eq!(fit(&history, 100_000), 0);
    }

    #[test]
    fn drops_whole_turns_oldest_first() {
        let mut history = turn("a", 40_000);
        history.extend(turn("b", 40_000));
        history.extend(turn("c", 40_000));

        // Room for roughly two turns of the three.
        let start = fit(&history, 22_000);
        assert_eq!(start, 3, "the oldest turn goes, on its boundary");
        assert_well_formed(&history[start..]);
    }

    #[test]
    fn the_newest_turn_survives_a_budget_it_cannot_fit() {
        let mut history = turn("a", 1_000);
        history.extend(turn("b", 400_000));

        let start = fit(&history, 1_000);
        assert_eq!(start, 3);
        assert_well_formed(&history[start..]);
    }

    #[test]
    fn a_conversation_of_plain_messages_trims_to_the_tail() {
        let history: Vec<Message> = (0..10)
            .flat_map(|i| {
                [Message::user(format!("q{i} {}", "x".repeat(4_000))), Message::assistant("a")]
            })
            .collect();

        let start = fit(&history, 3_000);
        assert!(start > 0, "a long conversation must be trimmed");
        assert_well_formed(&history[start..]);
        let kept: usize = history[start..].iter().map(cost).sum();
        assert!(kept <= 3_000, "kept {kept} tokens over a 3000 budget");
    }
}
