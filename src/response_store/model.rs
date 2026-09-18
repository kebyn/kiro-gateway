use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseStatus {
    InProgress,
    Completed,
    Incomplete,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub id: String,
    pub object: String,
    pub status: ResponseStatus,
    pub model: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ResponseEvent {
    pub response_id: String,
    pub sequence_number: u64,
    pub event_type: String,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}
