use crate::{
    error::AppError,
    response_store::model::{ResponseEvent, ResponseRecord, ResponseStatus},
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    path::Path,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub struct SqliteStore {
    conn: std::sync::Mutex<Connection>,
}
impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        prepare_store_file(path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(include_str!("../../migrations/001_initial.sql"))?;
        // The initial scaffold created a conversations table that was never
        // read or written. Remove it during upgrade so the schema reflects the
        // response_id-based continuation model instead of advertising a
        // second, unused conversation lifecycle.
        conn.execute("DROP TABLE IF EXISTS conversations", [])?;
        // Databases created by the early 0.1 releases had no per-response
        // sequence column. Upgrade that table in place so event reads remain
        // deterministic without reintroducing the unused conversations table.
        let has_sequence = conn
            .prepare("PRAGMA table_info(response_events)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|name| name == "sequence_number");
        if !has_sequence {
            conn.execute("ALTER TABLE response_events ADD COLUMN sequence_number INTEGER", [])?;
            let mut rows = conn
                .prepare("SELECT id, response_id FROM response_events ORDER BY response_id, id")?;
            let values = rows
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut current = String::new();
            let mut sequence = 0_i64;
            for (id, response_id) in values {
                if response_id != current {
                    current = response_id.clone();
                    sequence = 0;
                }
                conn.execute(
                    "UPDATE response_events SET sequence_number = ?1 WHERE id = ?2",
                    params![sequence, id],
                )?;
                sequence += 1;
            }
            conn.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_response_events_sequence ON response_events(response_id, sequence_number)",
                [],
            )?;
        }
        Ok(Self { conn: std::sync::Mutex::new(conn) })
    }
    pub fn put(&self, record: &ResponseRecord) -> Result<(), AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        conn.execute(
            "INSERT INTO responses(id, object, status, model, payload, created_at, updated_at) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(id) DO UPDATE SET object=excluded.object, status=excluded.status, model=excluded.model, payload=excluded.payload, created_at=excluded.created_at, updated_at=excluded.updated_at",
            params![record.id, record.object, serde_json::to_string(&record.status)?, record.model, serde_json::to_string(&record.payload)?, record.created_at.timestamp(), record.updated_at.timestamp()],
        )?;
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
    ) -> Result<ResponseEvent, AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        let sequence: i64 = conn.query_row(
            "SELECT COALESCE(MAX(sequence_number) + 1, 0) FROM response_events WHERE response_id = ?1",
            [response_id],
            |row| row.get(0),
        )?;
        let created_at = Utc::now();
        let mut stored_payload = payload.clone();
        if let Value::Object(object) = &mut stored_payload {
            object.insert("type".into(), Value::String(event_type.to_owned()));
            object.insert("sequence_number".into(), Value::from(sequence));
        }
        conn.execute(
            "INSERT INTO response_events(response_id, sequence_number, event_type, payload, created_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![response_id, sequence, event_type, serde_json::to_string(&stored_payload)?, created_at.timestamp()],
        )?;
        Ok(ResponseEvent {
            response_id: response_id.to_owned(),
            sequence_number: sequence as u64,
            event_type: event_type.to_owned(),
            payload: stored_payload,
            created_at,
        })
    }

    pub fn events(&self, response_id: &str) -> Result<Vec<ResponseEvent>, AppError> {
        let conn = self.conn.lock().map_err(|_| AppError::Storage("store lock poisoned".into()))?;
        let mut statement = conn.prepare(
            "SELECT response_id, sequence_number, event_type, payload, created_at FROM response_events WHERE response_id = ?1 ORDER BY sequence_number ASC",
        )?;
        let rows = statement.query_map([response_id], |row| {
            let payload: String = row.get(3)?;
            Ok(ResponseEvent {
                response_id: row.get(0)?,
                sequence_number: row.get::<_, i64>(1)? as u64,
                event_type: row.get(2)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                created_at: DateTime::from_timestamp(row.get::<_, i64>(4)?, 0)
                    .unwrap_or_else(Utc::now),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(AppError::from)
    }
}

fn prepare_store_file(path: &Path) -> Result<(), AppError> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(path).map_err(|error| AppError::Storage(error.to_string()))?;
            file.sync_all().map_err(|error| AppError::Storage(error.to_string()))?;
        }
        Err(error) => return Err(AppError::Storage(error.to_string())),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::Storage(error.to_string()))?;
    Ok(())
}
