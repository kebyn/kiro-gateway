use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::{
    error::AppError,
    response_store::model::{ResponseEvent, ResponseRecord, ResponseStatus},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

const SCHEMA_VERSION: i64 = 2;

pub struct SqliteStore {
    conn: Connection,
    path: PathBuf,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        prepare_store_file(path)?;
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        initialize_or_validate_schema(&conn)?;
        harden_store_file(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        harden_sidecars(path);
        Ok(Self { conn, path: path.to_owned() })
    }

    pub fn create(&mut self, record: &ResponseRecord) -> Result<(), AppError> {
        let transaction = self.conn.transaction()?;
        insert_record(&transaction, record)?;
        write_payload_parts(&transaction, &record.id, &record.payload)?;
        transaction.commit()?;
        harden_sidecars(&self.path);
        Ok(())
    }

    pub fn transition(
        &mut self,
        record: &ResponseRecord,
        events: &[(String, Value)],
    ) -> Result<Vec<ResponseEvent>, AppError> {
        let transaction = self.conn.transaction()?;
        update_record(&transaction, record)?;
        write_payload_parts(&transaction, &record.id, &record.payload)?;
        let stored_events = append_events(&transaction, &record.id, events)?;
        transaction.commit()?;
        harden_sidecars(&self.path);
        Ok(stored_events)
    }

    pub fn transition_if_in_progress(
        &mut self,
        response_id: &str,
        status: ResponseStatus,
        payload: &Value,
        events: &[(String, Value)],
    ) -> Result<bool, AppError> {
        let transaction = self.conn.transaction()?;
        let current_status = transaction
            .query_row("SELECT status FROM responses WHERE id = ?1", [response_id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        let Some(current_status) = current_status else {
            return Ok(false);
        };
        if decode_status(&current_status)? != ResponseStatus::InProgress {
            return Ok(false);
        }
        transaction.execute(
            "UPDATE responses SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![encode_status(status)?, Utc::now().timestamp(), response_id],
        )?;
        write_payload_parts(&transaction, response_id, payload)?;
        append_events(&transaction, response_id, events)?;
        transaction.commit()?;
        harden_sidecars(&self.path);
        Ok(true)
    }

    pub fn get(&self, id: &str) -> Result<Option<ResponseRecord>, AppError> {
        let row = self
            .conn
            .query_row(
                "SELECT r.id, r.object, r.status, r.model, q.payload, s.payload, r.created_at, r.updated_at
                 FROM responses r
                 JOIN response_requests q ON q.response_id = r.id
                 LEFT JOIN response_snapshots s ON s.response_id = r.id
                 WHERE r.id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, object, status, model, request, snapshot, created_at, updated_at)) = row
        else {
            return Ok(None);
        };
        let request = decode_json(&request, "response request")?;
        let snapshot =
            snapshot.as_deref().map(|value| decode_json(value, "response snapshot")).transpose()?;
        Ok(Some(ResponseRecord {
            id,
            object,
            status: decode_status(&status)?,
            model,
            payload: join_payload(request, snapshot),
            created_at: timestamp(created_at)?,
            updated_at: timestamp(updated_at)?,
        }))
    }

    pub fn delete(&mut self, id: &str) -> Result<bool, AppError> {
        let changed = self.conn.execute("DELETE FROM responses WHERE id = ?1", [id])? > 0;
        if changed {
            harden_sidecars(&self.path);
        }
        Ok(changed)
    }

    pub fn add_event(
        &mut self,
        response_id: &str,
        event_type: &str,
        payload: &Value,
    ) -> Result<ResponseEvent, AppError> {
        let transaction = self.conn.transaction()?;
        let mut events =
            append_events(&transaction, response_id, &[(event_type.to_owned(), payload.clone())])?;
        transaction.commit()?;
        harden_sidecars(&self.path);
        events.pop().ok_or_else(|| AppError::Storage("response event was not stored".into()))
    }

    pub fn events(&self, response_id: &str) -> Result<Vec<ResponseEvent>, AppError> {
        let mut statement = self.conn.prepare(
            "SELECT response_id, sequence_number, event_type, payload, created_at
             FROM response_events WHERE response_id = ?1 ORDER BY sequence_number ASC",
        )?;
        let rows = statement.query_map([response_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (response_id, sequence_number, event_type, payload, created_at) = row?;
            Ok(ResponseEvent {
                response_id,
                sequence_number: u64::try_from(sequence_number)
                    .map_err(|_| AppError::Storage("negative response event sequence".into()))?,
                event_type,
                payload: decode_json(&payload, "response event")?,
                created_at: timestamp(created_at)?,
            })
        })
        .collect()
    }
}

fn initialize_or_validate_schema(conn: &Connection) -> Result<(), AppError> {
    let mut statement = conn.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )?;
    let tables =
        statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if tables.is_empty() {
        conn.execute_batch(include_str!("../../migrations/002_response_store_v2.sql"))?;
        return Ok(());
    }
    if !tables.iter().any(|table| table == "schema_meta") {
        return Err(AppError::Storage(
            "legacy Responses database detected; v2 does not migrate or overwrite it. Back it up and configure a new storage path"
                .into(),
        ));
    }
    let versions = conn
        .prepare("SELECT version FROM schema_meta")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if versions.as_slice() != [SCHEMA_VERSION] {
        return Err(AppError::Storage(format!(
            "unsupported Responses database schema version; expected {SCHEMA_VERSION}"
        )));
    }
    for required in ["responses", "response_requests", "response_snapshots", "response_events"] {
        if !tables.iter().any(|table| table == required) {
            return Err(AppError::Storage(format!(
                "Responses database schema v2 is missing required table {required}"
            )));
        }
    }
    Ok(())
}

fn insert_record(transaction: &Transaction<'_>, record: &ResponseRecord) -> Result<(), AppError> {
    transaction.execute(
        "INSERT INTO responses(id, object, status, model, created_at, updated_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            record.id,
            record.object,
            encode_status(record.status)?,
            record.model,
            record.created_at.timestamp(),
            record.updated_at.timestamp()
        ],
    )?;
    Ok(())
}

fn update_record(transaction: &Transaction<'_>, record: &ResponseRecord) -> Result<(), AppError> {
    let changed = transaction.execute(
        "UPDATE responses SET object = ?1, status = ?2, model = ?3, updated_at = ?4
         WHERE id = ?5",
        params![
            record.object,
            encode_status(record.status)?,
            record.model,
            record.updated_at.timestamp(),
            record.id
        ],
    )?;
    if changed != 1 {
        return Err(AppError::Storage(format!("response {} does not exist", record.id)));
    }
    Ok(())
}

fn write_payload_parts(
    transaction: &Transaction<'_>,
    response_id: &str,
    payload: &Value,
) -> Result<(), AppError> {
    let (request, snapshot) = split_payload(payload);
    transaction.execute(
        "INSERT INTO response_requests(response_id, payload) VALUES(?1, ?2)
         ON CONFLICT(response_id) DO UPDATE SET payload = excluded.payload",
        params![response_id, encode_json(&request, "response request")?],
    )?;
    if let Some(snapshot) = snapshot {
        transaction.execute(
            "INSERT INTO response_snapshots(response_id, payload) VALUES(?1, ?2)
             ON CONFLICT(response_id) DO UPDATE SET payload = excluded.payload",
            params![response_id, encode_json(&snapshot, "response snapshot")?],
        )?;
    } else {
        transaction
            .execute("DELETE FROM response_snapshots WHERE response_id = ?1", [response_id])?;
    }
    Ok(())
}

fn append_events(
    transaction: &Transaction<'_>,
    response_id: &str,
    events: &[(String, Value)],
) -> Result<Vec<ResponseEvent>, AppError> {
    let mut sequence: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sequence_number) + 1, 0)
         FROM response_events WHERE response_id = ?1",
        [response_id],
        |row| row.get(0),
    )?;
    let mut stored = Vec::with_capacity(events.len());
    for (event_type, payload) in events {
        let created_at = Utc::now();
        let mut stored_payload = payload.clone();
        if let Value::Object(object) = &mut stored_payload {
            object.insert("type".into(), Value::String(event_type.clone()));
            object.insert("sequence_number".into(), Value::from(sequence));
        }
        transaction.execute(
            "INSERT INTO response_events(response_id, sequence_number, event_type, payload, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5)",
            params![
                response_id,
                sequence,
                event_type,
                encode_json(&stored_payload, "response event")?,
                created_at.timestamp()
            ],
        )?;
        stored.push(ResponseEvent {
            response_id: response_id.to_owned(),
            sequence_number: u64::try_from(sequence)
                .map_err(|_| AppError::Storage("negative response event sequence".into()))?,
            event_type: event_type.clone(),
            payload: stored_payload,
            created_at,
        });
        sequence = sequence.saturating_add(1);
    }
    Ok(stored)
}

fn split_payload(payload: &Value) -> (Value, Option<Value>) {
    match payload {
        Value::Object(object) => {
            let mut request = object.clone();
            let snapshot = request.remove("response");
            (Value::Object(request), snapshot)
        }
        value => (value.clone(), None),
    }
}

fn join_payload(request: Value, snapshot: Option<Value>) -> Value {
    match request {
        Value::Object(mut object) => {
            if let Some(snapshot) = snapshot {
                object.insert("response".into(), snapshot);
            }
            Value::Object(object)
        }
        value => value,
    }
}

fn encode_status(status: ResponseStatus) -> Result<String, AppError> {
    serde_json::to_string(&status)
        .map_err(|error| AppError::Storage(format!("serialize response status: {error}")))
}

fn decode_status(status: &str) -> Result<ResponseStatus, AppError> {
    serde_json::from_str(status)
        .map_err(|error| AppError::Storage(format!("invalid stored response status: {error}")))
}

fn encode_json(value: &Value, label: &str) -> Result<String, AppError> {
    serde_json::to_string(value)
        .map_err(|error| AppError::Storage(format!("serialize {label}: {error}")))
}

fn decode_json(value: &str, label: &str) -> Result<Value, AppError> {
    serde_json::from_str(value)
        .map_err(|error| AppError::Storage(format!("invalid stored {label}: {error}")))
}

fn timestamp(value: i64) -> Result<DateTime<Utc>, AppError> {
    DateTime::from_timestamp(value, 0)
        .ok_or_else(|| AppError::Storage("invalid stored response timestamp".into()))
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
    Ok(())
}

fn harden_store_file(path: &Path) -> Result<(), AppError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| AppError::Storage(error.to_string()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
fn harden_sidecars(path: &Path) {
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", path.display(), suffix));
        if sidecar.exists() {
            let _ = fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o600));
        }
    }
}

#[cfg(not(unix))]
fn harden_sidecars(_path: &Path) {}
