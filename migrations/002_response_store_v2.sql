CREATE TABLE schema_meta (
    version INTEGER NOT NULL CHECK (version = 2)
);

INSERT INTO schema_meta(version) VALUES (2);

CREATE TABLE responses (
    id TEXT PRIMARY KEY,
    object TEXT NOT NULL,
    status TEXT NOT NULL,
    model TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE response_requests (
    response_id TEXT PRIMARY KEY,
    payload TEXT NOT NULL,
    FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE
);

CREATE TABLE response_snapshots (
    response_id TEXT PRIMARY KEY,
    payload TEXT NOT NULL,
    FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE
);

CREATE TABLE response_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    response_id TEXT NOT NULL,
    sequence_number INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE,
    UNIQUE(response_id, sequence_number)
);

CREATE INDEX idx_response_events_response_id
    ON response_events(response_id);
