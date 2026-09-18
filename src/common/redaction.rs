pub fn redact(value: &str) -> String {
    let value = value.trim();
    if value.len() <= 8 {
        return "[REDACTED]".to_owned();
    }
    format!("{}…{}", &value[..4], &value[value.len() - 4..])
}

pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let sensitive = [
                    "token",
                    "access_token",
                    "refresh_token",
                    "client_secret",
                    "api_key",
                    "authorization",
                    "cookie",
                ]
                .iter()
                .any(|needle| key.to_ascii_lowercase().contains(needle));
                if sensitive {
                    *item = serde_json::Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json(item);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json),
        _ => {}
    }
}
