-- Persistent counters are independent of daily demo queue resets.
CREATE TABLE IF NOT EXISTS policy_counters (id TEXT PRIMARY KEY, payload TEXT NOT NULL);
-- Immutable create body/hash and normalized, bound provider observations survive reset.
CREATE TABLE IF NOT EXISTS immutable_requests (id TEXT PRIMARY KEY, payload TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS provider_evidence (id TEXT PRIMARY KEY, payload TEXT NOT NULL);
