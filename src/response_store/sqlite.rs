use crate::{
    error::AppError,
    response_store::model::{ResponseRecord, ResponseStatus},
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::Path;

pub struct SqliteStore {
    conn: std::sync::Mutex<Connection>,
}
impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(include_str!("../../migrations/001_initial.sql"))?;
        Ok(Self { conn: std::sync::Mutex::new(conn) })
    }
    pub fn put(&self, record: &ResponseRecord) -> Result<(), AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        conn.execute("INSERT OR REPLACE INTO responses(id, object, status, model, payload, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![record.id, record.object, serde_json::to_string(&record.status)?, record.model, serde_json::to_string(&record.payload)?, record.created_at.timestamp(), record.updated_at.timestamp()])?;
        Ok(())
    }
    pub fn get(&self, id: &str) -> Result<Option<ResponseRecord>, AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        conn.query_row("SELECT id, object, status, model, payload, created_at, updated_at FROM responses WHERE id = ?1", [id], |row| {
            let status: String = row.get(2)?; let payload: String = row.get(4)?; Ok(ResponseRecord { id: row.get(0)?, object: row.get(1)?, status: serde_json::from_str(&status).unwrap_or(ResponseStatus::Failed), model: row.get(3)?, payload: serde_json::from_str(&payload).unwrap_or(Value::Null), created_at: DateTime::from_timestamp(row.get::<_, i64>(5)?, 0).unwrap_or_else(Utc::now), updated_at: DateTime::from_timestamp(row.get::<_, i64>(6)?, 0).unwrap_or_else(Utc::now) })
        }).optional().map_err(AppError::from)
    }
    pub fn delete(&self, id: &str) -> Result<bool, AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        Ok(conn.execute("DELETE FROM responses WHERE id = ?1", [id])? > 0)
    }
    pub fn add_event(
        &self,
        response_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<(), AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        conn.execute("INSERT INTO response_events(response_id, event_type, payload, created_at) VALUES(?1, ?2, ?3, ?4)", params![response_id, event_type, serde_json::to_string(payload)?, Utc::now().timestamp()])?;
        Ok(())
    }
}
