pub fn redact_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let key = key.to_ascii_lowercase();
                let sensitive = key == "token"
                    || key.ends_with("_token")
                    || key.contains("api_key")
                    || key.ends_with("_secret")
                    || key == "authorization"
                    || key == "cookie";
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

#[cfg(test)]
mod tests {
    use super::redact_json;

    #[test]
    fn redacts_nested_credentials_without_touching_safe_values() {
        let mut value = serde_json::json!({
            "client_api_key": "client-secret",
            "nested": {"refresh_token": "refresh-secret", "name": "kiro"},
            "items": [{"authorization": "Bearer secret", "cookie_secure": true}]
        });
        redact_json(&mut value);
        assert_eq!(value["client_api_key"], "[REDACTED]");
        assert_eq!(value["nested"]["refresh_token"], "[REDACTED]");
        assert_eq!(value["items"][0]["authorization"], "[REDACTED]");
        assert_eq!(value["items"][0]["cookie_secure"], true);
        assert_eq!(value["nested"]["name"], "kiro");
    }
}
