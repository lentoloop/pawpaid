-- Raw bytes and header values are losslessly encoded as JSON byte arrays.
CREATE TABLE IF NOT EXISTS webhook_inbox (id TEXT PRIMARY KEY, payload TEXT NOT NULL);
-- Key is environment + provider event ID; payload contains the original body hash.
CREATE TABLE IF NOT EXISTS sandbox_events (id TEXT PRIMARY KEY, payload TEXT NOT NULL);
