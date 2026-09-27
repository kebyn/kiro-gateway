use std::{path::Path, thread};

use chrono::Utc;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
#[cfg(test)]
use uuid::Uuid;

use crate::{
    error::AppError,
    generation::{Message, OpaqueHistory, ToolDefinition},
    response_store::{
        model::{ResponseEvent, ResponseRecord, ResponseStatus},
        sqlite::SqliteStore,
    },
};

type Reply<T> = oneshot::Sender<Result<T, AppError>>;

enum Command {
    Create {
        record: ResponseRecord,
        reply: Reply<ResponseRecord>,
    },
    Transition {
        record: ResponseRecord,
        events: Vec<(String, Value)>,
        reply: Reply<(ResponseRecord, Vec<ResponseEvent>)>,
    },
    TransitionIfInProgress {
        response_id: String,
        status: ResponseStatus,
        payload: Value,
        events: Vec<(String, Value)>,
    },
    Get {
        id: String,
        reply: Reply<Option<ResponseRecord>>,
    },
    Delete {
        id: String,
        reply: Reply<bool>,
    },
    AppendEvent {
        response_id: String,
        event_type: String,
        payload: Value,
        reply: Reply<ResponseEvent>,
    },
    Events {
        response_id: String,
        reply: Reply<Vec<ResponseEvent>>,
    },
}

#[derive(Clone)]
pub struct ResponseStore {
    commands: mpsc::UnboundedSender<Command>,
}

impl ResponseStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref().to_owned();
        let (commands, receiver) = mpsc::unbounded_channel();
        let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("kiro-response-store".into())
            .spawn(move || run_actor(&path, receiver, startup_tx))
            .map_err(|error| AppError::Storage(format!("start response store actor: {error}")))?;
        startup_rx.recv().map_err(|_| {
            AppError::Storage("response store actor stopped during startup".into())
        })??;
        Ok(Self { commands })
    }

    #[cfg(test)]
    pub fn unavailable_for_tests() -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        Self { commands }
    }

    #[cfg(test)]
    pub async fn create(
        &self,
        model: &str,
        payload: Value,
        status: ResponseStatus,
    ) -> Result<ResponseRecord, AppError> {
        self.create_with_id(format!("resp_{}", Uuid::now_v7()), model, payload, status).await
    }

    pub async fn create_with_id(
        &self,
        id: String,
        model: &str,
        payload: Value,
        status: ResponseStatus,
    ) -> Result<ResponseRecord, AppError> {
        let now = Utc::now();
        let record = ResponseRecord {
            id,
            object: "response".into(),
            status,
            model: model.into(),
            payload,
            created_at: now,
            updated_at: now,
        };
        let (reply, receive) = oneshot::channel();
        self.send(Command::Create { record, reply })?;
        receive_reply(receive).await
    }

    pub async fn transition(
        &self,
        mut record: ResponseRecord,
        status: ResponseStatus,
        payload: Value,
        events: Vec<(String, Value)>,
    ) -> Result<(ResponseRecord, Vec<ResponseEvent>), AppError> {
        record.status = status;
        record.payload = payload;
        record.updated_at = Utc::now();
        let (reply, receive) = oneshot::channel();
        self.send(Command::Transition { record, events, reply })?;
        receive_reply(receive).await
    }

    /// Queues a terminal transition without waiting. This is safe to call from
    /// a stream drop guard and never performs SQLite work on the Tokio worker.
    pub fn mark_incomplete_on_disconnect(
        &self,
        response_id: String,
        payload: Value,
        event_payload: Value,
    ) -> Result<(), AppError> {
        self.send(Command::TransitionIfInProgress {
            response_id,
            status: ResponseStatus::Incomplete,
            payload,
            events: vec![("response.incomplete".into(), event_payload)],
        })
    }

    pub async fn get(&self, id: &str) -> Result<Option<ResponseRecord>, AppError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Get { id: id.to_owned(), reply })?;
        receive_reply(receive).await
    }

    pub async fn delete(&self, id: &str) -> Result<bool, AppError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Delete { id: id.to_owned(), reply })?;
        receive_reply(receive).await
    }

    pub async fn append_event(
        &self,
        response_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<ResponseEvent, AppError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::AppendEvent {
            response_id: response_id.to_owned(),
            event_type: event_type.to_owned(),
            payload: payload.clone(),
            reply,
        })?;
        receive_reply(receive).await
    }

    pub async fn events(&self, response_id: &str) -> Result<Vec<ResponseEvent>, AppError> {
        let (reply, receive) = oneshot::channel();
        self.send(Command::Events { response_id: response_id.to_owned(), reply })?;
        receive_reply(receive).await
    }

    pub fn extract_messages(record: &ResponseRecord) -> Result<Vec<Message>, AppError> {
        extract_history_field(record, "messages")
    }

    pub fn extract_tools(record: &ResponseRecord) -> Result<Vec<ToolDefinition>, AppError> {
        extract_history_field(record, "tools")
    }

    pub fn extract_opaque_history(record: &ResponseRecord) -> Result<Vec<OpaqueHistory>, AppError> {
        extract_history_field(record, "opaque_history")
    }

    fn send(&self, command: Command) -> Result<(), AppError> {
        self.commands
            .send(command)
            .map_err(|_| AppError::Storage("response store actor is unavailable".into()))
    }
}

fn extract_history_field<T>(record: &ResponseRecord, field: &str) -> Result<Vec<T>, AppError>
where
    T: serde::de::DeserializeOwned,
{
    let value = record.payload.get(field).ok_or_else(|| {
        AppError::Storage(format!("stored response is missing required history field {field}"))
    })?;
    serde_json::from_value(value.clone()).map_err(|error| {
        AppError::Storage(format!("invalid stored response history field {field}: {error}"))
    })
}

async fn receive_reply<T>(receive: oneshot::Receiver<Result<T, AppError>>) -> Result<T, AppError> {
    receive
        .await
        .map_err(|_| AppError::Storage("response store actor stopped before replying".into()))?
}

fn run_actor(
    path: &Path,
    mut commands: mpsc::UnboundedReceiver<Command>,
    startup: std::sync::mpsc::SyncSender<Result<(), AppError>>,
) {
    let mut store = match SqliteStore::open(path) {
        Ok(store) => {
            let _ = startup.send(Ok(()));
            store
        }
        Err(error) => {
            let _ = startup.send(Err(error));
            return;
        }
    };
    while let Some(command) = commands.blocking_recv() {
        match command {
            Command::Create { record, reply } => {
                let result = store.create(&record).map(|()| record);
                let _ = reply.send(result);
            }
            Command::Transition { record, events, reply } => {
                let result = store.transition(&record, &events).map(|stored| (record, stored));
                let _ = reply.send(result);
            }
            Command::TransitionIfInProgress { response_id, status, payload, events } => {
                if let Err(error) =
                    store.transition_if_in_progress(&response_id, status, &payload, &events)
                {
                    tracing::warn!(response_id, error = %error, "failed to persist dropped response stream");
                }
            }
            Command::Get { id, reply } => {
                let _ = reply.send(store.get(&id));
            }
            Command::Delete { id, reply } => {
                let _ = reply.send(store.delete(&id));
            }
            Command::AppendEvent { response_id, event_type, payload, reply } => {
                let _ = reply.send(store.add_event(&response_id, &event_type, &payload));
            }
            Command::Events { response_id, reply } => {
                let _ = reply.send(store.events(&response_id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[tokio::test]
    async fn response_round_trips_and_deletes() {
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record = store
            .create(
                "kiro",
                serde_json::json!({"messages": [], "response": {"status":"in_progress"}}),
                ResponseStatus::InProgress,
            )
            .await
            .unwrap();
        let stored = store.get(&record.id).await.unwrap().unwrap();
        assert_eq!(stored.status, ResponseStatus::InProgress);
        assert_eq!(stored.payload["response"]["status"], "in_progress");
        assert!(store.delete(&record.id).await.unwrap());
        assert!(store.get(&record.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn events_are_ordered_and_cascade_on_delete() {
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record =
            store.create("kiro", serde_json::json!({}), ResponseStatus::InProgress).await.unwrap();
        let first = store
            .append_event(&record.id, "response.created", &serde_json::json!({"ok":true}))
            .await
            .unwrap();
        let second = store
            .append_event(&record.id, "response.completed", &serde_json::json!({"ok":true}))
            .await
            .unwrap();
        assert_eq!(first.sequence_number, 0);
        assert_eq!(second.sequence_number, 1);
        let events = store.events(&record.id).await.unwrap();
        assert_eq!(
            events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>(),
            ["response.created", "response.completed"]
        );
        assert_eq!(events[1].payload["sequence_number"], 1);
        let (updated, stored_events) = store
            .transition(
                record,
                ResponseStatus::Completed,
                serde_json::json!({"updated":true}),
                vec![("response.final".into(), serde_json::json!({"done":true}))],
            )
            .await
            .unwrap();
        assert_eq!(updated.status, ResponseStatus::Completed);
        assert_eq!(stored_events[0].sequence_number, 2);
        assert_eq!(store.events(&updated.id).await.unwrap().len(), 3);
        assert!(store.delete(&updated.id).await.unwrap());
        assert!(store.events(&updated.id).await.unwrap().is_empty());
    }

    #[test]
    fn rejects_legacy_database_without_modifying_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection.execute_batch(include_str!("../../migrations/001_initial.sql")).unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let error = ResponseStore::open(&path).err().expect("legacy schema must be rejected");
        assert!(error.to_string().contains("legacy Responses database"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn rejects_v2_database_with_missing_column_without_modifying_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE schema_meta(version INTEGER NOT NULL CHECK(version = 2));
                     INSERT INTO schema_meta(version) VALUES (2);
                     CREATE TABLE responses(id TEXT PRIMARY KEY, object TEXT NOT NULL,
                         status TEXT NOT NULL, model TEXT NOT NULL, created_at INTEGER NOT NULL);
                     CREATE TABLE response_requests(response_id TEXT PRIMARY KEY, payload TEXT NOT NULL,
                         FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE);
                     CREATE TABLE response_snapshots(response_id TEXT PRIMARY KEY, payload TEXT NOT NULL,
                         FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE);
                     CREATE TABLE response_events(id INTEGER PRIMARY KEY AUTOINCREMENT,
                         response_id TEXT NOT NULL, sequence_number INTEGER NOT NULL,
                         event_type TEXT NOT NULL, payload TEXT NOT NULL, created_at INTEGER NOT NULL,
                         FOREIGN KEY(response_id) REFERENCES responses(id) ON DELETE CASCADE,
                         UNIQUE(response_id, sequence_number));",
                )
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let error = ResponseStore::open(&path).err().expect("malformed v2 schema must be rejected");
        assert!(error.to_string().contains("invalid columns"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn corrupted_history_is_reported_instead_of_becoming_empty() {
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record = store
            .create(
                "kiro",
                serde_json::json!({
                    "messages": "not-an-array",
                    "tools": [],
                    "opaque_history": []
                }),
                ResponseStatus::Completed,
            )
            .await
            .unwrap();
        let record = store.get(&record.id).await.unwrap().unwrap();
        let error = ResponseStore::extract_messages(&record).unwrap_err();
        assert!(error.to_string().contains("messages"));

        let missing = store
            .create(
                "kiro",
                serde_json::json!({"messages": [], "tools": []}),
                ResponseStatus::Completed,
            )
            .await
            .unwrap();
        let missing = store.get(&missing.id).await.unwrap().unwrap();
        let error = ResponseStore::extract_opaque_history(&missing).unwrap_err();
        assert!(error.to_string().contains("opaque_history"));
    }

    #[tokio::test]
    async fn unavailable_actor_returns_observable_storage_errors() {
        let store = ResponseStore::unavailable_for_tests();
        let error = store.get("resp_missing").await.unwrap_err();
        assert!(matches!(error, AppError::Storage(message) if message.contains("unavailable")));
        let error = store
            .append_event("resp_missing", "response.created", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(error, AppError::Storage(message) if message.contains("unavailable")));
    }

    #[test]
    fn schema_v2_separates_requests_snapshots_and_events() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        let _store = ResponseStore::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let version: i64 =
            connection.query_row("SELECT version FROM schema_meta", [], |row| row.get(0)).unwrap();
        assert_eq!(version, 2);
        for table in ["responses", "response_requests", "response_snapshots", "response_events"] {
            let found: Option<String> = connection
                .query_row(
                    "SELECT name FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(found.as_deref(), Some(table));
        }
        let response_columns = connection
            .prepare("PRAGMA table_info(responses)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!response_columns.iter().any(|column| column == "payload"));
    }

    #[cfg(unix)]
    #[test]
    fn response_store_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        let _store = ResponseStore::open(&path).unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_sidecars_are_owner_only_when_present() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        let store = ResponseStore::open(&path).unwrap();
        let _ = store
            .create(
                "kiro",
                serde_json::json!({"messages": [], "tools": [], "opaque_history": []}),
                ResponseStatus::InProgress,
            )
            .await
            .unwrap();
        for suffix in ["-wal", "-shm"] {
            let sidecar = std::path::PathBuf::from(format!("{}{}", path.display(), suffix));
            if sidecar.exists() {
                assert_eq!(std::fs::metadata(sidecar).unwrap().permissions().mode() & 0o777, 0o600);
            }
        }
    }

    #[test]
    fn sqlite_uses_wal_and_busy_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        let _store = ResponseStore::open(&path).unwrap();
        let connection = rusqlite::Connection::open(path).unwrap();
        let journal: String =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap();
        let synchronous: i64 =
            connection.query_row("PRAGMA synchronous", [], |row| row.get(0)).unwrap();
        assert_eq!(journal.to_ascii_lowercase(), "wal");
        assert!(synchronous >= 1);
    }
}
