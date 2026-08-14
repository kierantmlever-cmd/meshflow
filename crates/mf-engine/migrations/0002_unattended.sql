-- Auto-approve mode: record whether anyone was actually asked.
--
-- `approved = 1` alone cannot answer the question the audit log exists for. A write that the user
-- read and allowed and a write that ran because auto-approve was on are the same row without
-- this, and the difference is the whole point of looking.
ALTER TABLE audit_log ADD COLUMN unattended INTEGER NOT NULL DEFAULT 0;
