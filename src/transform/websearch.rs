use serde_json::Value;
pub fn is_web_search_tool(tool: &Value) -> bool {
    tool.get("type")
        .and_then(Value::as_str)
        .is_some_and(|v| v == "web_search" || v == "web_search_preview")
}
