use crate::{
    error::AppError,
    protocol::internal::{InternalMessage, InternalTool},
    response_store::{
        model::{ResponseEvent, ResponseRecord, ResponseStatus},
        sqlite::SqliteStore,
    },
};
use chrono::Utc;
use serde_json::Value;
use std::{path::Path, sync::Arc};
#[cfg(test)]
use uuid::Uuid;

#[derive(Clone)]
pub struct ResponseStore {
    inner: Arc<SqliteStore>,
}
impl ResponseStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        Ok(Self { inner: Arc::new(SqliteStore::open(path.as_ref())?) })
    }
    #[cfg(test)]
    pub fn create(
        &self,
        model: &str,
        payload: Value,
        status: ResponseStatus,
    ) -> Result<ResponseRecord, AppError> {
        self.create_with_id(format!("resp_{}", Uuid::now_v7()), model, payload, status)
    }
    pub fn create_with_id(
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
        self.inner.put(&record)?;
        Ok(record)
    }
    pub fn update(
        &self,
        mut record: ResponseRecord,
        status: ResponseStatus,
        payload: Value,
    ) -> Result<ResponseRecord, AppError> {
        record.status = status;
        record.payload = payload;
        record.updated_at = Utc::now();
        self.inner.put(&record)?;
        Ok(record)
    }
    pub fn get(&self, id: &str) -> Result<Option<ResponseRecord>, AppError> {
        self.inner.get(id)
    }
    pub fn delete(&self, id: &str) -> Result<bool, AppError> {
        self.inner.delete(id)
    }
    pub fn append_event(
        &self,
        response_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<ResponseEvent, AppError> {
        self.inner.add_event(response_id, event_type, payload)
    }
    pub fn events(&self, response_id: &str) -> Result<Vec<ResponseEvent>, AppError> {
        self.inner.events(response_id)
    }
    pub fn extract_messages(record: &ResponseRecord) -> Vec<InternalMessage> {
        record
            .payload
            .get("messages")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }
    pub fn extract_tools(record: &ResponseRecord) -> Vec<InternalTool> {
        record
            .payload
            .get("tools")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OptionalExtension;

    #[test]
    fn response_round_trips_and_deletes() {
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record = store
            .create("kiro", serde_json::json!({"messages": []}), ResponseStatus::InProgress)
            .unwrap();
        assert_eq!(store.get(&record.id).unwrap().unwrap().status, ResponseStatus::InProgress);
        assert!(store.delete(&record.id).unwrap());
        assert!(store.get(&record.id).unwrap().is_none());
    }

    #[test]
    fn events_are_ordered_and_cascade_on_delete() {
        let directory = tempfile::tempdir().unwrap();
        let store = ResponseStore::open(directory.path().join("responses.sqlite3")).unwrap();
        let record =
            store.create("kiro", serde_json::json!({}), ResponseStatus::InProgress).unwrap();
        let first = store
            .append_event(&record.id, "response.created", &serde_json::json!({"ok":true}))
            .unwrap();
        let second = store
            .append_event(&record.id, "response.completed", &serde_json::json!({"ok":true}))
            .unwrap();
        assert_eq!(first.sequence_number, 0);
        assert_eq!(second.sequence_number, 1);
        let events = store.events(&record.id).unwrap();
        assert_eq!(
            events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>(),
            ["response.created", "response.completed"]
        );
        assert_eq!(events[1].payload["sequence_number"], 1);
        let updated = store
            .update(
                store.get(&record.id).unwrap().unwrap(),
                ResponseStatus::Completed,
                serde_json::json!({"updated":true}),
            )
            .unwrap();
        assert_eq!(updated.status, ResponseStatus::Completed);
        assert_eq!(store.events(&record.id).unwrap().len(), 2);
        assert!(store.delete(&record.id).unwrap());
        assert!(store.events(&record.id).unwrap().is_empty());
    }

    #[test]
    fn schema_does_not_keep_unused_conversations_table() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.sqlite3");
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute("CREATE TABLE conversations (id TEXT PRIMARY KEY, payload TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL)", [])
                .unwrap();
        }
        let _store = ResponseStore::open(&path).unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let table: Option<String> = connection
            .query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='conversations'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(table.is_none());
    }
}
