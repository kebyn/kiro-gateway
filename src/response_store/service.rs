use crate::{
    error::AppError,
    protocol::internal::{InternalMessage, InternalResponse},
    response_store::{
        model::{ResponseRecord, ResponseStatus},
        sqlite::SqliteStore,
    },
};
use chrono::Utc;
use serde_json::Value;
use std::{path::Path, sync::Arc};
use uuid::Uuid;

#[derive(Clone)]
pub struct ResponseStore {
    inner: Arc<SqliteStore>,
}
impl ResponseStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        Ok(Self { inner: Arc::new(SqliteStore::open(path.as_ref())?) })
    }
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
    pub fn event(
        &self,
        response_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), AppError> {
        self.inner.add_event(response_id, event_type, payload)
    }
    pub fn extract_messages(record: &ResponseRecord) -> Vec<InternalMessage> {
        record
            .payload
            .get("messages")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    }
    pub fn response_payload(response: &InternalResponse) -> Value {
        serde_json::json!({"output_text": response.text, "thinking": response.thinking, "tool_calls": response.tool_calls, "usage": response.usage, "stop_reason": response.stop_reason, "incomplete": response.incomplete})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
