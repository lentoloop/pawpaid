-- Lossless archive for verified webhook deliveries. Rows move here from webhook_inbox in one
-- transaction (full payload, body and headers intact) so the live inbox stays bounded without
-- discarding verified provider audit evidence. Nothing deletes from this table.
CREATE TABLE IF NOT EXISTS webhook_inbox_archive (
    id TEXT PRIMARY KEY,
    payload TEXT NOT NULL,
    status TEXT NOT NULL,
    received BIGINT NOT NULL,
    claim_id TEXT,
    archived_at BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS inbox_archive_claim ON webhook_inbox_archive(claim_id, status);
