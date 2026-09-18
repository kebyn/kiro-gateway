use serde_json::Value;

pub fn compress_schema(schema: &Value, max_bytes: usize) -> Value {
    let mut compact = schema.clone();
    if serde_json::to_vec(&compact).map(|v| v.len()).unwrap_or(0) <= max_bytes {
        return compact;
    }
    if let Some(object) = compact.as_object_mut() {
        object.remove("description");
        object.remove("examples");
        object.remove("additionalProperties");
    }
    compact
}
