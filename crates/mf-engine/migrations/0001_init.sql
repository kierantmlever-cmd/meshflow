-- Phase 1 schema: enough to persist a conversation and audit what tools did.
--
-- Deliberately *not* here: workspaces, agents, memories, index_files. Those tables belong to
-- features that don't exist yet, and an empty table is a promise the code hasn't made.

CREATE TABLE conversations (
    id          TEXT PRIMARY KEY NOT NULL,
    title       TEXT,
    created_at  TEXT NOT NULL
) STRICT;

CREATE TABLE messages (
    id              TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    -- 'user' | 'assistant' | 'tool'
    role            TEXT NOT NULL,
    -- Monotonic per conversation. Ordering by timestamp breaks when two messages land in the
    -- same millisecond, which tool results routinely do.
    ord             INTEGER NOT NULL,
    created_at      TEXT NOT NULL
) STRICT;

CREATE UNIQUE INDEX messages_conv_ord ON messages (conversation_id, ord);

-- One row per content block. A message is a sequence of parts (text, a tool call, a tool
-- result), so storing a single TEXT body would lose tool structure on reload.
CREATE TABLE message_parts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id  TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    ord         INTEGER NOT NULL,
    -- 'text' | 'tool_call' | 'tool_result'
    kind        TEXT NOT NULL,
    -- Text content, or the tool result body.
    text        TEXT,
    -- tool_call: the arguments object. Null for text parts.
    json        TEXT,
    -- tool_call / tool_result: the provider's call id, used to pair them back up.
    tool_use_id TEXT,
    tool_name   TEXT,
    is_error    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX message_parts_message ON message_parts (message_id, ord);

-- The security record. Written *before* a tool runs, so an action that crashes the app is still
-- attributable. Never contains secrets.
CREATE TABLE audit_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          TEXT NOT NULL,
    -- 'tool' | 'denied' | 'approval'
    action      TEXT NOT NULL,
    tool        TEXT,
    -- Rendered operation, as shown to the user for approval.
    detail      TEXT,
    approved    INTEGER,
    elevated    INTEGER NOT NULL DEFAULT 0,
    ok          INTEGER
) STRICT;

CREATE INDEX audit_log_ts ON audit_log (ts);
