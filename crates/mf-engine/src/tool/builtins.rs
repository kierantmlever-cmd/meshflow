//! The Phase 1 toolset.
//!
//! None of these check permissions — [`ToolRegistry::dispatch`](super::ToolRegistry::dispatch)
//! does that before they are reached. They *do* run every path through [`PathPolicy`], because the
//! policy needs the resolved path anyway and returning it is what closes the TOCTOU gap.

use std::process::Stdio;

use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, process::Command};

use super::{Permission, PreviewKind, Tool, ToolCtx, ToolError, ToolOutput, ToolPreview, arg_str};
use crate::fsaccess::Op;

/// Cap on what a tool may return. A model that reads a 40MB minified bundle blows the context
/// window and the request fails far from the cause.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Shared with [`crate::context`]: one cap on how much text any single blob may inject into a
/// request, whether a tool produced it or the user attached it with `@`.
pub(crate) fn truncate(mut s: String, what: &str) -> String {
    if s.len() > MAX_OUTPUT_BYTES {
        // Cut on a char boundary; `s` may be UTF-8 and slicing mid-codepoint panics.
        let mut end = MAX_OUTPUT_BYTES;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str(&format!("\n\n[… {what} truncated at {MAX_OUTPUT_BYTES} bytes]"));
    }
    s
}

pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file. Paths may be relative to the working directory."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to read." }
            },
            "required": ["path"]
        })
    }

    fn permission(&self) -> Permission {
        Permission::READ
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let raw = arg_str(&args, "path", self.name())?;
        let path = ctx.policy.check(std::path::Path::new(&raw), Op::Read)?;

        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| ToolError::Failed(format!("reading {}: {e}", path.display())))?;

        // Binary files are worse than useless to a model: they eat the context window and can
        // break the JSON encoding of the response.
        let text = String::from_utf8(bytes).map_err(|_| {
            ToolError::Failed(format!("{} is not UTF-8 text", path.display()))
        })?;

        let lines = text.lines().count();
        Ok(ToolOutput::with_summary(
            truncate(text, "file"),
            format!("read {} ({lines} lines)", path.display()),
        ))
    }
}

pub struct WriteFile;

impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a text file. Requires user approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to write." },
                "content": { "type": "string", "description": "Full new contents." }
            },
            "required": ["path", "content"]
        })
    }

    fn permission(&self) -> Permission {
        Permission::WRITE
    }

    /// Always. Overwriting a file is destructive and unrecoverable without VCS.
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    /// Show the *change*, not just the new text.
    ///
    /// Rendering only the incoming content asks the user to consent to destroying a file they
    /// cannot see. For anything that already exists, what is actually being approved is the diff.
    async fn preview(&self, ctx: &ToolCtx, args: &Value) -> ToolPreview {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or("<missing>");
        let content = args.get("content").and_then(Value::as_str).unwrap_or("");

        // Resolve first, so the prompt names the file that will actually be written rather than
        // whatever string the model produced.
        let path = match ctx.policy.check(std::path::Path::new(raw), Op::Write) {
            Ok(path) => path,
            // Worth showing rather than silently previewing a write that cannot happen: the user
            // would otherwise approve it and see an unexplained failure.
            Err(denied) => {
                return ToolPreview::text(
                    format!("Write {raw}"),
                    format!("This write will be refused.\n\n{denied}"),
                );
            }
        };
        let shown = path.display().to_string();

        match tokio::fs::read_to_string(&path).await {
            Ok(existing) => {
                let diff = crate::diff::unified(&existing, content);
                if diff.is_empty() {
                    return ToolPreview::text(
                        format!("Write {shown}"),
                        "The file already has exactly these contents. Nothing will change.",
                    );
                }
                let tally = diff.tally();
                ToolPreview {
                    title: format!("Overwrite {shown}  ({tally})"),
                    detail: diff.text,
                    summary: format!("overwrite {shown} ({tally})"),
                    kind: PreviewKind::Diff,
                }
            }

            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Nothing to diff against, so show the content itself — capped, because a prompt
                // the user cannot read is not consent either.
                let lines = content.lines().count();
                let shown_body: String = content.lines().take(60).collect::<Vec<_>>().join("\n");
                let body = match lines.saturating_sub(60) {
                    0 => shown_body,
                    more => format!("{shown_body}\n… {more} more lines"),
                };
                ToolPreview {
                    title: format!("Create {shown}  ({lines} lines)"),
                    detail: if body.is_empty() { "(empty file)".into() } else { body },
                    summary: format!("create {shown} ({lines} lines)"),
                    kind: PreviewKind::Text,
                }
            }

            // Existing file that is not UTF-8. There is no meaningful diff, and that is exactly
            // the case where quietly showing the new text would hide what is being destroyed.
            Err(_) => ToolPreview::text(
                format!("Replace {shown}"),
                format!(
                    "The existing file is not UTF-8 text, so no diff can be shown. It will be \
                     replaced with {} bytes of new content.",
                    content.len()
                ),
            ),
        }
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let raw = arg_str(&args, "path", self.name())?;
        let content = arg_str(&args, "content", self.name())?;
        let path = ctx.policy.check(std::path::Path::new(&raw), Op::Write)?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| ToolError::Failed(format!("creating {}: {e}", parent.display())))?;
        }
        tokio::fs::write(&path, &content)
            .await
            .map_err(|e| ToolError::Failed(format!("writing {}: {e}", path.display())))?;

        let bytes = content.len();
        Ok(ToolOutput::new(format!("Wrote {bytes} bytes to {}", path.display())))
    }
}

pub struct ListDir;

impl Tool for ListDir {
    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> &'static str {
        "List the entries of a directory. Directories are suffixed with '/'."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list. Defaults to '.'." }
            }
        })
    }

    fn permission(&self) -> Permission {
        Permission::READ
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let dir = ctx.policy.check(std::path::Path::new(raw), Op::Read)?;

        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .map_err(|e| ToolError::Failed(format!("listing {}: {e}", dir.display())))?;

        let mut names = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ToolError::Failed(format!("listing {}: {e}", dir.display())))?
        {
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            let name = entry.file_name().to_string_lossy().into_owned();
            names.push(if is_dir { format!("{name}/") } else { name });
        }
        names.sort();

        let count = names.len();
        Ok(ToolOutput::with_summary(
            truncate(names.join("\n"), "listing"),
            format!("listed {} ({count} entries)", dir.display()),
        ))
    }
}

pub struct SearchFiles;

impl Tool for SearchFiles {
    fn name(&self) -> &'static str {
        "search_files"
    }

    fn description(&self) -> &'static str {
        "Search the workspace. Returns matching lines as 'path:line: text'. The query is literal \
         text unless `regex` is true, in which case it is a Rust regular expression matched \
         against each line on its own — so `^` and `$` anchor to the line."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Text or pattern to find." },
                "path": {
                    "type": "string",
                    "description": "Directory to search under. Defaults to the workspace root."
                },
                "case_sensitive": { "type": "boolean", "description": "Defaults to false." },
                "regex": {
                    "type": "boolean",
                    "description": "Treat the query as a regular expression. Defaults to false."
                }
            },
            "required": ["query"]
        })
    }

    fn permission(&self) -> Permission {
        Permission::READ
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let query = super::arg_str(&args, "query", self.name())?;
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let query = crate::search::Query {
            text: query,
            case_sensitive: args.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false),
            regex: args.get("regex").and_then(Value::as_bool).unwrap_or(false),
        };

        // A pattern the model got wrong comes back as a tool error it can read and fix, which is
        // the same treatment a denied path gets.
        let results =
            crate::search::search(&ctx.policy, std::path::Path::new(raw), query.clone())
                .await
                .map_err(ToolError::Failed)?;

        if results.hits.is_empty() {
            // Said plainly rather than returned as an empty string: a blank tool result reads to
            // a model as a failure, and it retries the same search instead of moving on.
            return Ok(ToolOutput::with_summary(
                format!("No matches for `{}` in {} files.", query.text, results.files_searched),
                format!("searched {} files, no matches", results.files_searched),
            ));
        }

        let body = results
            .hits
            .iter()
            .map(|h| format!("{}:{}: {}", h.path.display(), h.line, h.text))
            .collect::<Vec<_>>()
            .join("\n");
        let note = if results.truncated { " (truncated)" } else { "" };

        Ok(ToolOutput::with_summary(
            truncate(body, "results"),
            format!("{} matches for `{}`{note}", results.hits.len(), query.text),
        ))
    }
}

pub struct RunCommand;

impl Tool for RunCommand {
    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Run a shell command in the working directory and return its output. Requires approval."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Command line to execute." }
            },
            "required": ["command"]
        })
    }

    fn permission(&self) -> Permission {
        Permission::EXEC
    }

    /// Always. There is no safe subset of shell worth allowlisting by pattern — `rm` hides inside
    /// `make clean`, and a heuristic that is wrong once is worse than no heuristic.
    fn needs_approval(&self, _args: &Value) -> bool {
        true
    }

    async fn preview(&self, _ctx: &ToolCtx, args: &Value) -> ToolPreview {
        // Verbatim, unedited. The user is approving *this* string.
        ToolPreview::text(
            "Run command",
            args.get("command").and_then(Value::as_str).unwrap_or("<missing>"),
        )
    }

    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let command = arg_str(&args, "command", self.name())?;

        let shell = if cfg!(windows) { "cmd" } else { "sh" };
        let flag = if cfg!(windows) { "/C" } else { "-c" };

        let mut child = Command::new(shell)
            .arg(flag)
            .arg(&command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Without a group the timeout kills the shell and orphans its children.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Failed(format!("spawning {shell}: {e}")))?;

        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");

        let run = async {
            let mut out = String::new();
            let mut err = String::new();
            // Concurrent reads: a command that fills the stderr pipe deadlocks if we drain
            // stdout to completion first.
            let (a, b) = tokio::join!(stdout.read_to_string(&mut out), stderr.read_to_string(&mut err));
            a.map_err(|e| ToolError::Failed(format!("reading stdout: {e}")))?;
            b.map_err(|e| ToolError::Failed(format!("reading stderr: {e}")))?;
            let status = child
                .wait()
                .await
                .map_err(|e| ToolError::Failed(format!("waiting for command: {e}")))?;
            Ok::<_, ToolError>((status, out, err))
        };

        let (status, out, err) = match tokio::time::timeout(COMMAND_TIMEOUT, run).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(ToolError::Failed(format!(
                    "command exceeded {}s and was killed: {command}",
                    COMMAND_TIMEOUT.as_secs()
                )));
            }
        };

        let code = status.code().unwrap_or(-1);
        let mut body = String::new();
        if !out.is_empty() {
            body.push_str(&out);
        }
        if !err.is_empty() {
            // Labelled, because a model given bare interleaved output cannot tell which stream
            // an error line came from.
            body.push_str("\n[stderr]\n");
            body.push_str(&err);
        }
        if body.trim().is_empty() {
            body = format!("(no output, exit code {code})");
        }

        Ok(ToolOutput::with_summary(
            truncate(format!("exit code {code}\n{body}"), "output"),
            format!("`{}` exited {code}", command.lines().next().unwrap_or(&command)),
        ))
    }
}

/// Hand a task to a sub-agent with a narrower role.
pub struct Delegate;

/// The roles a task can be delegated to, and the permissions each implies.
///
/// A closed set, not a permission list the model composes: letting a model assemble the bit
/// pattern for its own sub-agent is asking it to grant itself `EXEC`, and it will.
const ROLES: &[(&str, Permission)] = &[
    ("research", Permission::RESEARCH),
    ("coding", Permission::CODING),
    ("documentation", Permission::DOCUMENTATION),
];

impl Tool for Delegate {
    fn name(&self) -> &'static str {
        "delegate"
    }

    fn description(&self) -> &'static str {
        "Hand one self-contained task to a sub-agent and get its final answer back. The sub-agent \
         sees only the task text — none of this conversation — so describe the job completely. \
         Roles: research (read and web), coding (read, write, run commands), documentation \
         (read and write). The sub-agent never gets a permission you do not already hold."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "role": {
                    "type": "string",
                    "enum": ROLES.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                    "description": "Which role the sub-agent takes."
                },
                "task": {
                    "type": "string",
                    "description": "The complete task, including any context the sub-agent needs."
                }
            },
            "required": ["role", "task"]
        })
    }

    fn permission(&self) -> Permission {
        Permission::AGENT
    }

    /// No approval of its own. A sub-agent's destructive calls each stop at the same modal this
    /// one would — approving the *delegation* would be consenting to work nobody has described
    /// yet, which is worse than not asking.
    async fn call(&self, ctx: &ToolCtx, args: Value) -> Result<ToolOutput, ToolError> {
        let role = arg_str(&args, "role", self.name())?;
        let task = arg_str(&args, "task", self.name())?;

        let Some((name, permission)) = ROLES.iter().find(|(name, _)| *name == role) else {
            return Err(ToolError::BadArguments {
                tool: self.name().to_owned(),
                reason: format!(
                    "unknown role `{role}` — use one of: {}",
                    ROLES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "),
                ),
            });
        };

        let Some(delegate) = &ctx.delegate else {
            return Err(ToolError::Failed(
                "delegation is not available here — this tool only works inside an agent run"
                    .into(),
            ));
        };

        let answer = delegate
            .run(task, name, *permission, ctx.depth + 1)
            .await
            .map_err(ToolError::Failed)?;

        Ok(ToolOutput::with_summary(
            truncate(answer, "answer"),
            format!("delegated to a {role} agent"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fsaccess::{AccessMode, PathPolicy},
        tool::{Approval, Permission, ToolRegistry},
    };

    fn sandbox() -> (tempfile::TempDir, ToolCtx) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let ctx = ToolCtx {
            policy: PathPolicy::new(AccessMode::WorkspaceSandbox, [root.clone()], true),
            cwd: root,
            depth: 0,
            delegate: None,
        };
        (tmp, ctx)
    }

    #[tokio::test]
    async fn read_write_round_trip_through_the_registry() {
        let (tmp, ctx) = sandbox();
        let registry = ToolRegistry::with_builtins();
        let path = tmp.path().join("hello.txt");

        registry
            .dispatch(
                "write_file",
                json!({ "path": path.to_str().unwrap(), "content": "hello\nworld\n" }),
                &ctx,
                Permission::all(),
                Some(Approval::Allow),
            )
            .await
            .expect("approved write succeeds");

        let out = registry
            .dispatch(
                "read_file",
                json!({ "path": path.to_str().unwrap() }),
                &ctx,
                Permission::all(),
                None,
            )
            .await
            .expect("read succeeds");

        assert_eq!(out.content, "hello\nworld\n");
    }

    #[tokio::test]
    async fn tools_cannot_escape_the_sandbox() {
        let (_tmp, ctx) = sandbox();
        let registry = ToolRegistry::with_builtins();

        let err = registry
            .dispatch("read_file", json!({ "path": "/etc/passwd" }), &ctx, Permission::all(), None)
            .await
            .expect_err("reads outside the sandbox must fail");

        assert!(matches!(err, ToolError::Path(_)), "got {err:?}");
        // The message has to name the reason, or the model just retries the same path.
        assert!(err.to_string().contains("outside"), "{err}");
    }

    #[tokio::test]
    async fn write_is_refused_for_a_denied_pattern() {
        let (tmp, ctx) = sandbox();
        let registry = ToolRegistry::with_builtins();
        let env = tmp.path().join(".env");

        let err = registry
            .dispatch(
                "write_file",
                json!({ "path": env.to_str().unwrap(), "content": "TOKEN=leaked" }),
                &ctx,
                Permission::all(),
                Some(Approval::Allow),
            )
            .await
            .expect_err("even an approved write cannot touch a denied pattern");

        assert!(matches!(err, ToolError::Path(_)));
        assert!(!env.exists());
    }

    #[tokio::test]
    async fn binary_files_are_rejected_rather_than_mangled() {
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("blob.bin");
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();

        let err = ReadFile
            .call(&ctx, json!({ "path": path.to_str().unwrap() }))
            .await
            .expect_err("binary must not be fed to the model");
        assert!(err.to_string().contains("not UTF-8"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_command_captures_output_and_exit_code() {
        let (_tmp, ctx) = sandbox();
        let out = RunCommand
            .call(&ctx, json!({ "command": "echo out; echo err >&2; exit 3" }))
            .await
            .expect("command runs");

        assert!(out.content.contains("exit code 3"));
        assert!(out.content.contains("out"));
        assert!(out.content.contains("[stderr]"));
        assert!(out.content.contains("err"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_command_runs_in_the_sandbox_cwd() {
        let (tmp, ctx) = sandbox();
        std::fs::write(tmp.path().join("marker.txt"), "x").unwrap();

        let out = RunCommand.call(&ctx, json!({ "command": "ls" })).await.unwrap();
        assert!(out.content.contains("marker.txt"));
    }

    #[tokio::test]
    async fn oversized_output_is_truncated() {
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("big.txt");
        std::fs::write(&path, "x".repeat(MAX_OUTPUT_BYTES * 2)).unwrap();

        let out = ReadFile.call(&ctx, json!({ "path": path.to_str().unwrap() })).await.unwrap();
        assert!(out.content.contains("truncated"));
        assert!(out.content.len() < MAX_OUTPUT_BYTES + 200);
    }

    #[tokio::test]
    async fn list_dir_marks_directories() {
        let (tmp, ctx) = sandbox();
        std::fs::create_dir(tmp.path().join("subdir")).unwrap();
        std::fs::write(tmp.path().join("file.txt"), "x").unwrap();

        let out = ListDir.call(&ctx, json!({ "path": "." })).await.unwrap();
        assert!(out.content.contains("subdir/"));
        assert!(out.content.contains("file.txt"));
    }

    #[tokio::test]
    async fn creating_a_new_file_previews_its_contents() {
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("new.rs");
        let preview = WriteFile
            .preview(&ctx, &json!({ "path": path.to_str().unwrap(), "content": "fn main() {}" }))
            .await;

        assert!(preview.title.starts_with("Create"), "{}", preview.title);
        assert!(preview.detail.contains("fn main() {}"), "the user must see what they approve");
        assert_eq!(preview.kind, PreviewKind::Text, "nothing to diff against");
    }

    #[tokio::test]
    async fn overwriting_previews_the_diff_not_the_new_text() {
        // The point of the whole change: approving an overwrite while seeing only the incoming
        // content is consenting to destroy something you were never shown.
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    old();\n}\n").unwrap();

        let preview = WriteFile
            .preview(
                &ctx,
                &json!({
                    "path": path.to_str().unwrap(),
                    "content": "fn main() {\n    new();\n}\n"
                }),
            )
            .await;

        assert_eq!(preview.kind, PreviewKind::Diff);
        assert!(preview.title.contains("Overwrite"), "{}", preview.title);
        assert!(preview.title.contains("+1 −1"), "the tally belongs in the title: {}", preview.title);
        // Both sides present: what is being removed matters more than what replaces it.
        assert!(preview.detail.contains("-    old();"), "{}", preview.detail);
        assert!(preview.detail.contains("+    new();"), "{}", preview.detail);
    }

    #[tokio::test]
    async fn deleting_a_files_contents_shows_every_removed_line() {
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("doomed.txt");
        std::fs::write(&path, "keep\nthis\nsafe\n").unwrap();

        let preview =
            WriteFile.preview(&ctx, &json!({ "path": path.to_str().unwrap(), "content": "" })).await;

        assert!(preview.title.contains("−3"), "{}", preview.title);
        for line in ["-keep", "-this", "-safe"] {
            assert!(preview.detail.contains(line), "{line} missing from:\n{}", preview.detail);
        }
    }

    #[tokio::test]
    async fn a_no_op_write_says_so_rather_than_showing_an_empty_diff() {
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("same.txt");
        std::fs::write(&path, "unchanged\n").unwrap();

        let preview = WriteFile
            .preview(&ctx, &json!({ "path": path.to_str().unwrap(), "content": "unchanged\n" }))
            .await;

        assert!(preview.detail.contains("Nothing will change"), "{}", preview.detail);
    }

    #[tokio::test]
    async fn replacing_a_binary_file_says_no_diff_is_possible() {
        // Silently falling back to "here is the new text" would hide that something
        // unreadable-but-real is being destroyed.
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("blob.bin");
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

        let preview = WriteFile
            .preview(&ctx, &json!({ "path": path.to_str().unwrap(), "content": "text" }))
            .await;

        assert!(preview.title.contains("Replace"), "{}", preview.title);
        assert!(preview.detail.contains("not UTF-8"), "{}", preview.detail);
    }

    #[tokio::test]
    async fn a_write_the_policy_will_refuse_says_so_up_front() {
        // Approving a write that then fails for reasons never shown teaches users the prompt is
        // noise. Better to say it cannot happen while they are still reading.
        let (_tmp, ctx) = sandbox();
        let preview = WriteFile
            .preview(&ctx, &json!({ "path": "/etc/passwd", "content": "x" }))
            .await;

        assert!(preview.detail.contains("will be refused"), "{}", preview.detail);
        assert!(preview.detail.contains("outside"), "and why: {}", preview.detail);
    }

    #[tokio::test]
    async fn the_audit_summary_stays_one_line_even_for_a_huge_diff() {
        // `detail` can be 400 lines; the audit row must not be.
        let (tmp, ctx) = sandbox();
        let path = tmp.path().join("big.txt");
        std::fs::write(&path, (0..500).map(|i| format!("old {i}\n")).collect::<String>()).unwrap();

        let preview = WriteFile
            .preview(
                &ctx,
                &json!({
                    "path": path.to_str().unwrap(),
                    "content": (0..500).map(|i| format!("new {i}\n")).collect::<String>()
                }),
            )
            .await;

        assert_eq!(preview.summary.lines().count(), 1, "summary: {}", preview.summary);
        assert!(preview.summary.contains("+500 −500"), "{}", preview.summary);
    }

    /// Stands in for the engine, recording what the tool asked it to run.
    #[derive(Default)]
    struct Recorder {
        calls: std::sync::Mutex<Vec<(String, &'static str, Permission, u8)>>,
    }

    impl super::super::Delegator for Recorder {
        fn run<'a>(
            &'a self,
            task: String,
            role: &'static str,
            permission: Permission,
            depth: u8,
        ) -> futures::future::BoxFuture<'a, Result<String, String>> {
            self.calls.lock().unwrap().push((task, role, permission, depth));
            Box::pin(async { Ok("the sub-agent's answer".to_owned()) })
        }
    }

    #[tokio::test]
    async fn delegate_maps_the_role_and_descends_one_level() {
        let (_tmp, mut ctx) = sandbox();
        let recorder = std::sync::Arc::new(Recorder::default());
        ctx.depth = 2;
        ctx.delegate = Some(recorder.clone());

        let out = Delegate
            .call(&ctx, json!({ "role": "research", "task": "find the release date" }))
            .await
            .unwrap();

        assert_eq!(out.content, "the sub-agent's answer");
        let calls = recorder.calls.lock().unwrap();
        assert_eq!(calls[0].0, "find the release date");
        // The role travels as a name too — it is what the approval modal says out loud.
        assert_eq!(calls[0].1, "research");
        assert_eq!(calls[0].2, Permission::RESEARCH);
        // One deeper than the caller, which is what the registry's cap counts.
        assert_eq!(calls[0].3, 3, "delegation must descend, or the depth cap never bites");
    }

    #[tokio::test]
    async fn an_unknown_role_names_the_ones_that_exist() {
        let (_tmp, mut ctx) = sandbox();
        ctx.delegate = Some(std::sync::Arc::new(Recorder::default()));

        let err = Delegate
            .call(&ctx, json!({ "role": "sysadmin", "task": "do it" }))
            .await
            .unwrap_err();

        // The model can fix this from the message alone, which is the point.
        let message = err.to_string();
        assert!(message.contains("research"), "{message}");
        assert!(message.contains("coding"), "{message}");
    }

    #[tokio::test]
    async fn delegating_without_an_engine_behind_it_fails_loudly() {
        // The editor and the search panel share these tools' context and have nothing to run a
        // sub-agent with; silently returning nothing would read to a model as a done task.
        let (_tmp, ctx) = sandbox();
        let err = Delegate.call(&ctx, json!({ "role": "coding", "task": "x" })).await.unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }

    #[tokio::test]
    async fn command_preview_is_verbatim() {
        let (_tmp, ctx) = sandbox();
        let preview = RunCommand.preview(&ctx, &json!({ "command": "rm -rf ./build" })).await;
        assert_eq!(preview.detail, "rm -rf ./build");
        assert_eq!(preview.kind, PreviewKind::Text);
    }
}
