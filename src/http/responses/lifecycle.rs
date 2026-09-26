use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{
    generation::{Message, ToolDefinition},
    response_store::{ResponseStatus, ResponseStore},
};

#[derive(Clone)]
pub(super) struct ResponseSnapshot {
    pub(super) messages: Vec<Message>,
    pub(super) tools: Vec<ToolDefinition>,
    pub(super) response: Value,
    pub(super) status: ResponseStatus,
}

pub(super) struct IncompleteRecordGuard {
    pub(super) store: Option<ResponseStore>,
    pub(super) record_id: String,
    pub(super) snapshot: Arc<Mutex<ResponseSnapshot>>,
}

impl Drop for IncompleteRecordGuard {
    fn drop(&mut self) {
        let Some(store) = self.store.as_ref() else { return };
        let Ok(Some(record)) = store.get(&self.record_id) else { return };
        if record.status != ResponseStatus::InProgress {
            return;
        }
        let snapshot = self.snapshot.lock().clone();
        if snapshot.status != ResponseStatus::InProgress {
            return;
        }
        let mut payload = snapshot.response;
        payload["status"] = json!("incomplete");
        payload["error"] = json!({"code":"client_disconnected","message":"client disconnected before response completion"});
        payload["incomplete_details"] = json!({"reason":"client_disconnect"});
        let _ = store.update(
            record,
            ResponseStatus::Incomplete,
            json!({"messages":snapshot.messages,"tools":snapshot.tools,"response":payload}),
        );
        let _ = store.append_event(&self.record_id, "response.incomplete", &payload);
    }
}
