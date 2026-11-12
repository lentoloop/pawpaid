-- Retry scheduling and bounded provider-call reservations stay outside the ledger snapshot.
CREATE TABLE IF NOT EXISTS inbox_schedule (
    id TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    received BIGINT NOT NULL,
    due BIGINT NOT NULL,
    attempts BIGINT NOT NULL,
    claim_id TEXT
);
CREATE INDEX IF NOT EXISTS inbox_due ON inbox_schedule(status, due, received);
CREATE INDEX IF NOT EXISTS inbox_claim ON inbox_schedule(claim_id, status);
CREATE TABLE IF NOT EXISTS inbox_budget (
    id BIGINT PRIMARY KEY,
    minute BIGINT NOT NULL,
    used BIGINT NOT NULL
);
