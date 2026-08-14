//! Persistence: sqlite at `~/.local/share/meshflow/meshflow.db`.
//!
//! Holds conversations, their messages, and the audit log. **Never holds secrets** — API keys
//! live in the OS keychain ([`crate::secrets`]), and this file is treated as readable by anything
//! that can read the user's home directory.
//!
//! Queries are runtime-checked (`sqlx::query`, not `query!`) so building the project never needs
//! a live `DATABASE_URL`.

use std::path::Path;

use sqlx::{
    Row,
    sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions},
};

use crate::{
    proto::ConvId,
    provider::{Message, Part, Role},
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("migration: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("could not locate a data directory for this platform")]
    NoDataDir,
    #[error("creating {path}: {source}")]
    Io { path: String, source: std::io::Error },
}

static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    /// Open the user's database, creating and migrating it if needed.
    pub async fn open_default() -> Result<Self, StoreError> {
        let dir = crate::paths::data_dir().ok_or(StoreError::NoDataDir)?;
        std::fs::create_dir_all(&dir)
            .map_err(|source| StoreError::Io { path: dir.display().to_string(), source })?;
        Self::open(&dir.join("meshflow.db")).await
    }

    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // WAL so a long read (loading a big transcript) doesn't block the writes an active
            // run is making.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .foreign_keys(true);

        // sqlite takes a write lock for the whole database, so extra writer connections buy
        // contention rather than throughput. Reads are fast enough not to need more.
        Self::from_options(options, 4).await
    }

    /// In-memory database, for tests.
    pub async fn open_memory() -> Result<Self, StoreError> {
        // Exactly one connection, and not for performance: each connection to `:memory:` gets
        // its **own private database**, so a pooled in-memory store would run the migrations on
        // one connection and then query an empty schema on the next.
        Self::from_options(SqliteConnectOptions::new().in_memory(true).foreign_keys(true), 1).await
    }

    async fn from_options(
        options: SqliteConnectOptions,
        max_connections: u32,
    ) -> Result<Self, StoreError> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(options)
            .await?;

        MIGRATIONS.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn create_conversation(&self, id: ConvId, title: &str) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO conversations (id, title, created_at) VALUES (?, ?, ?)")
            .bind(id.to_string())
            .bind(title)
            .bind(now())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Append a message and its parts.
    ///
    /// Both go in one transaction: a message row without its parts would load back as an empty
    /// turn and silently corrupt the conversation sent to the provider on the next request.
    pub async fn append_message(
        &self,
        conv: ConvId,
        message: &Message,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;

        // Allocate the next slot inside the transaction so two concurrent appends can't collide
        // on (conversation_id, ord).
        let ord: i64 = sqlx::query(
            "SELECT COALESCE(MAX(ord), -1) + 1 FROM messages WHERE conversation_id = ?",
        )
        .bind(conv.to_string())
        .fetch_one(&mut *tx)
        .await?
        .get(0);

        let message_id = uuid::Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, role, ord, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&message_id)
        .bind(conv.to_string())
        .bind(role_str(message.role))
        .bind(ord)
        .bind(now())
        .execute(&mut *tx)
        .await?;

        for (index, part) in message.content.iter().enumerate() {
            let (kind, text, json, tool_use_id, tool_name, is_error) = match part {
                Part::Text(t) => ("text", Some(t.clone()), None, None, None, false),
                Part::ToolCall { id, name, args } => (
                    "tool_call",
                    None,
                    Some(args.to_string()),
                    Some(id.clone()),
                    Some(name.clone()),
                    false,
                ),
                Part::ToolResult { id, content, is_error } => (
                    "tool_result",
                    Some(content.clone()),
                    None,
                    Some(id.clone()),
                    None,
                    *is_error,
                ),
            };

            sqlx::query(
                "INSERT INTO message_parts \
                 (message_id, ord, kind, text, json, tool_use_id, tool_name, is_error) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&message_id)
            .bind(index as i64)
            .bind(kind)
            .bind(text)
            .bind(json)
            .bind(tool_use_id)
            .bind(tool_name)
            .bind(is_error as i64)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Load a full transcript, in order, ready to send to a provider.
    pub async fn load_conversation(&self, conv: ConvId) -> Result<Vec<Message>, StoreError> {
        let rows = sqlx::query(
            "SELECT m.id, m.role, p.kind, p.text, p.json, p.tool_use_id, p.tool_name, p.is_error \
             FROM messages m \
             JOIN message_parts p ON p.message_id = m.id \
             WHERE m.conversation_id = ? \
             ORDER BY m.ord, p.ord",
        )
        .bind(conv.to_string())
        .fetch_all(&self.pool)
        .await?;

        let mut messages: Vec<Message> = Vec::new();
        // Grouped by message id, not by role: consecutive same-role turns are legal (and common
        // — a user sending two messages in a row), and keying on role would silently merge them
        // into one, corrupting the transcript sent back to the provider.
        let mut current_id: Option<String> = None;

        for row in rows {
            let id = row.get::<String, _>("id");
            let role = parse_role(row.get::<String, _>("role").as_str());
            let part = match row.get::<String, _>("kind").as_str() {
                "text" => Part::Text(row.get::<Option<String>, _>("text").unwrap_or_default()),
                "tool_call" => Part::ToolCall {
                    id: row.get::<Option<String>, _>("tool_use_id").unwrap_or_default(),
                    name: row.get::<Option<String>, _>("tool_name").unwrap_or_default(),
                    args: row
                        .get::<Option<String>, _>("json")
                        .and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_else(|| serde_json::Value::Object(Default::default())),
                },
                _ => Part::ToolResult {
                    id: row.get::<Option<String>, _>("tool_use_id").unwrap_or_default(),
                    content: row.get::<Option<String>, _>("text").unwrap_or_default(),
                    is_error: row.get::<i64, _>("is_error") != 0,
                },
            };

            // The join flattens parts, so regroup them: a row belonging to a different message
            // starts a new turn.
            if current_id.as_deref() == Some(id.as_str()) {
                messages.last_mut().expect("id set implies a message").content.push(part);
            } else {
                messages.push(Message { role, content: vec![part] });
                current_id = Some(id);
            }
        }

        Ok(messages)
    }

    pub async fn list_conversations(&self) -> Result<Vec<(ConvId, String)>, StoreError> {
        let rows =
            sqlx::query("SELECT id, title FROM conversations ORDER BY created_at DESC")
                .fetch_all(&self.pool)
                .await?;

        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id = r.get::<String, _>("id").parse().ok().map(ConvId)?;
                Some((id, r.get::<Option<String>, _>("title").unwrap_or_default()))
            })
            .collect())
    }

    /// Record a tool invocation. Called *before* execution so an action that crashes the process
    /// is still attributable afterwards.
    pub async fn audit(&self, entry: AuditEntry<'_>) -> Result<i64, StoreError> {
        let id = sqlx::query(
            "INSERT INTO audit_log (ts, action, tool, detail, approved, unattended, elevated, ok) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now())
        .bind(entry.action)
        .bind(entry.tool)
        .bind(entry.detail)
        .bind(entry.approved.map(|a| a as i64))
        .bind(entry.unattended as i64)
        .bind(entry.elevated as i64)
        .bind(entry.ok.map(|o| o as i64))
        .execute(&self.pool)
        .await?
        .last_insert_rowid();
        Ok(id)
    }

    /// Fill in the outcome of an audited action once it finishes.
    pub async fn audit_complete(&self, id: i64, ok: bool) -> Result<(), StoreError> {
        sqlx::query("UPDATE audit_log SET ok = ? WHERE id = ?")
            .bind(ok as i64)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn recent_audit(&self, limit: i64) -> Result<Vec<AuditRow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, ts, action, tool, detail, approved, unattended, elevated, ok \
             FROM audit_log ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| AuditRow {
                id: r.get("id"),
                ts: r.get("ts"),
                action: r.get("action"),
                tool: r.get("tool"),
                detail: r.get("detail"),
                approved: r.get::<Option<i64>, _>("approved").map(|v| v != 0),
                unattended: r.get::<i64, _>("unattended") != 0,
                elevated: r.get::<i64, _>("elevated") != 0,
                ok: r.get::<Option<i64>, _>("ok").map(|v| v != 0),
            })
            .collect())
    }
}

pub struct AuditEntry<'a> {
    pub action: &'a str,
    pub tool: Option<&'a str>,
    pub detail: Option<&'a str>,
    pub approved: Option<bool>,
    /// True when the call ran without a prompt — auto-approve mode, or a tool the user had
    /// already waved through for the session. `approved = true` on its own cannot tell those
    /// apart from a decision someone actually read and made.
    pub unattended: bool,
    pub elevated: bool,
    pub ok: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct AuditRow {
    pub id: i64,
    pub ts: String,
    pub action: String,
    pub tool: Option<String>,
    pub detail: Option<String>,
    pub approved: Option<bool>,
    pub unattended: bool,
    pub elevated: bool,
    pub ok: Option<bool>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn parse_role(raw: &str) -> Role {
    match raw {
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn store() -> Store {
        Store::open_memory().await.expect("open in-memory store")
    }

    #[tokio::test]
    async fn migrations_apply_to_an_empty_database() {
        let store = store().await;
        // Reaching a query at all proves the schema exists.
        assert!(store.list_conversations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn round_trips_a_transcript_including_tool_structure() {
        let store = store().await;
        let conv = ConvId::new();
        store.create_conversation(conv, "test").await.unwrap();

        let turns = vec![
            Message::user("read a.rs"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Part::Text("Reading it.".into()),
                    Part::ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        args: json!({ "path": "a.rs" }),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![Part::ToolResult {
                    id: "call_1".into(),
                    content: "fn main() {}".into(),
                    is_error: false,
                }],
            },
        ];
        for turn in &turns {
            store.append_message(conv, turn).await.unwrap();
        }

        let loaded = store.load_conversation(conv).await.unwrap();
        assert_eq!(loaded.len(), 3, "turns must not be merged or split");

        // The tool call must survive with its id, name and arguments intact — a provider
        // rejects the next request if the call and its result don't pair up.
        let Part::ToolCall { id, name, args } = &loaded[1].content[1] else {
            panic!("expected a tool call, got {:?}", loaded[1].content[1]);
        };
        assert_eq!(id, "call_1");
        assert_eq!(name, "read_file");
        assert_eq!(args, &json!({ "path": "a.rs" }));

        let Part::ToolResult { id, content, is_error } = &loaded[2].content[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(id, "call_1");
        assert_eq!(content, "fn main() {}");
        assert!(!is_error);
    }

    #[tokio::test]
    async fn preserves_order_across_many_messages() {
        let store = store().await;
        let conv = ConvId::new();
        store.create_conversation(conv, "ordering").await.unwrap();

        for i in 0..25 {
            store.append_message(conv, &Message::user(format!("msg {i}"))).await.unwrap();
        }

        let loaded = store.load_conversation(conv).await.unwrap();
        // Consecutive same-role turns must stay distinct, and stay in order — ordering by
        // timestamp would scramble these, since they land in the same millisecond.
        assert_eq!(loaded.len(), 25);
        for (i, msg) in loaded.iter().enumerate() {
            let Part::Text(t) = &msg.content[0] else { panic!("expected text") };
            assert_eq!(t, &format!("msg {i}"));
        }
    }

    #[tokio::test]
    async fn conversations_are_isolated_from_each_other() {
        let store = store().await;
        let (a, b) = (ConvId::new(), ConvId::new());
        store.create_conversation(a, "a").await.unwrap();
        store.create_conversation(b, "b").await.unwrap();

        store.append_message(a, &Message::user("in a")).await.unwrap();
        store.append_message(b, &Message::user("in b")).await.unwrap();

        assert_eq!(store.load_conversation(a).await.unwrap().len(), 1);
        let Part::Text(t) = &store.load_conversation(b).await.unwrap()[0].content[0] else {
            panic!()
        };
        assert_eq!(t, "in b");
    }

    #[tokio::test]
    async fn audit_records_the_action_before_its_outcome_is_known() {
        let store = store().await;

        let id = store
            .audit(AuditEntry {
                action: "tool",
                tool: Some("run_command"),
                detail: Some("rm -rf ./build"),
                approved: Some(true),
                unattended: false,
                elevated: false,
                ok: None, // not finished yet
            })
            .await
            .unwrap();

        let pending = &store.recent_audit(10).await.unwrap()[0];
        assert_eq!(pending.tool.as_deref(), Some("run_command"));
        // The argv is recorded verbatim: an audit trail that summarises is not an audit trail.
        assert_eq!(pending.detail.as_deref(), Some("rm -rf ./build"));
        assert_eq!(pending.approved, Some(true));
        assert_eq!(pending.ok, None, "outcome is unknown until the tool returns");

        store.audit_complete(id, true).await.unwrap();
        assert_eq!(store.recent_audit(10).await.unwrap()[0].ok, Some(true));
    }

    #[tokio::test]
    async fn audit_is_newest_first_and_respects_its_limit() {
        let store = store().await;
        for i in 0..5 {
            store
                .audit(AuditEntry {
                    action: "tool",
                    tool: Some("read_file"),
                    detail: Some(&format!("file {i}")),
                    approved: None,
                    unattended: false,
                elevated: false,
                    ok: Some(true),
                })
                .await
                .unwrap();
        }

        let rows = store.recent_audit(3).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].detail.as_deref(), Some("file 4"));
    }

    #[tokio::test]
    async fn a_message_for_an_unknown_conversation_is_rejected() {
        let store = store().await;
        // Foreign keys are on, so an orphaned message can't be written — that would load back
        // as an invisible turn belonging to nothing.
        assert!(store.append_message(ConvId::new(), &Message::user("orphan")).await.is_err());
    }

    #[tokio::test]
    async fn survives_reopening_the_same_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meshflow.db");
        let conv = ConvId::new();

        {
            let store = Store::open(&path).await.unwrap();
            store.create_conversation(conv, "persisted").await.unwrap();
            store.append_message(conv, &Message::user("remember me")).await.unwrap();
        }

        // Reopening runs the migrations again; they must be idempotent.
        let store = Store::open(&path).await.unwrap();
        let loaded = store.load_conversation(conv).await.unwrap();
        let Part::Text(t) = &loaded[0].content[0] else { panic!() };
        assert_eq!(t, "remember me");
    }
}
