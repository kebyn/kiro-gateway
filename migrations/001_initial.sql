CREATE TABLE IF NOT EXISTS responses (
    id TEXT PRIMARY KEY,
    object TEXT NOT NULL,
    status TEXT NOT NULL,
    model TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS response_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    response_id TEXT NOT NULL,
    sequence_number INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE,
    UNIQUE(response_id, sequence_number)
);
CREATE INDEX IF NOT EXISTS idx_response_events_response_id ON response_events(response_id);
