use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::{
    generation::{Message, OpaqueHistory, ToolDefinition},
    response_store::{ResponseStatus, ResponseStore},
};

#[derive(Clone)]
pub(super) struct ResponseSnapshot {
    pub(super) messages: Vec<Message>,
    pub(super) tools: Vec<ToolDefinition>,
    pub(super) opaque_history: Vec<OpaqueHistory>,
    pub(super) response: Value,
    pub(super) status: ResponseStatus,
}

pub(super) struct IncompleteRecordGuard {
    pub(super) store: Option<ResponseStore>,
    pub(super) record_id: String,
    pub(super) snapshot: Arc<Mutex<ResponseSnapshot>>,
}

pub(super) fn set_snapshot_status(
    snapshot: &Arc<Mutex<ResponseSnapshot>>,
    status: ResponseStatus,
    response: Value,
) {
    let mut snapshot = snapshot.lock();
    snapshot.status = status;
    snapshot.response = response;
}

impl Drop for IncompleteRecordGuard {
    fn drop(&mut self) {
        let Some(store) = self.store.as_ref() else { return };
        let snapshot = self.snapshot.lock().clone();
        if snapshot.status != ResponseStatus::InProgress {
            return;
        }
        let mut payload = snapshot.response;
        payload["status"] = json!("incomplete");
        payload["error"] = json!({"code":"client_disconnected","message":"client disconnected before response completion"});
        payload["incomplete_details"] = json!({"reason":"client_disconnect"});
        let event_payload = json!({"response":payload.clone()});
        if let Err(error) = store.mark_incomplete_on_disconnect(
            self.record_id.clone(),
            json!({
                "messages":snapshot.messages,
                "tools":snapshot.tools,
                "opaque_history":snapshot.opaque_history,
                "response":payload
            }),
            event_payload,
        ) {
            tracing::error!(
                response_id = %self.record_id,
                error = %error,
                "failed to queue client-disconnect response transition"
            );
        }
    }
}
