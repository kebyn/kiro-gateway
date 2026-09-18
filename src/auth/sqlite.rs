use super::{AuthMethod, Credential, SecretString};
use crate::error::AppError;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;
use std::path::Path;

const TOKEN_KEYS: &[&str] =
    &["kirocli:social:token", "kirocli:odic:token", "codewhisperer:odic:token"];
const DEVICE_KEYS: &[&str] =
    &["kirocli:odic:device-registration", "codewhisperer:odic:device-registration"];

pub fn load(path: &Path) -> Result<Vec<Credential>, AppError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| AppError::Credential(format!("{}: {e}", path.display())))?;
    let mut result = Vec::new();
    for key in TOKEN_KEYS {
        let raw = read_any(&conn, key);
        if let Some(raw) = raw {
            if let Some(mut credential) = parse_token(&raw) {
                credential.auth_method =
                    if key.contains("social") { AuthMethod::Social } else { AuthMethod::Sso };
                credential.source = None;
                result.push(credential);
            }
        }
    }
    // Some CLI versions store a JSON object in a generic state table.
    if result.is_empty() {
        result.extend(load_state_tables(&conn));
    }
    if let Some(first) = result.first_mut() {
        if let Some(device) = DEVICE_KEYS.iter().find_map(|key| read_any(&conn, key)) {
            let registration = serde_json::from_str::<Value>(&device).ok();
            if let Some(value) = registration.as_ref() {
                if let Some(client_id) =
                    value.get("clientId").or_else(|| value.get("client_id")).and_then(Value::as_str)
                {
                    first.client_id = Some(SecretString::new(client_id));
                }
                if let Some(client_secret) = value
                    .get("clientSecret")
                    .or_else(|| value.get("client_secret"))
                    .and_then(Value::as_str)
                {
                    first.client_secret = Some(SecretString::new(client_secret));
                }
                if first.sso_region.is_none() {
                    first.sso_region =
                        value.get("region").and_then(Value::as_str).map(ToOwned::to_owned);
                }
            }
            let machine_id = registration.and_then(|v| {
                v.get("machineId")
                    .or_else(|| v.get("deviceId"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            });
            if let Some(id) = machine_id {
                first.machine_id = id;
            }
        }
        if let Ok(profile) = conn.query_row::<String, _, _>(
            "SELECT value FROM state WHERE key = 'api.codewhisperer.profile'",
            [],
            |row| row.get(0),
        ) {
            if let Ok(value) = serde_json::from_str::<Value>(&profile) {
                super::profile_resolver::resolve_profile(&value, first);
            }
        }
    }
    if result.is_empty() {
        return Err(AppError::Credential("no supported Kiro token found in SQLite".into()));
    }
    Ok(result)
}

fn read_any(conn: &Connection, key: &str) -> Option<String> {
    for sql in [
        "SELECT value FROM auth_kv WHERE key = ?1",
        "SELECT value FROM key_value WHERE key = ?1",
        "SELECT value FROM settings WHERE key = ?1",
        "SELECT data FROM state WHERE key = ?1",
    ] {
        if let Ok(v) = conn.query_row(sql, [key], |row| row.get::<_, String>(0)).optional() {
            if v.is_some() {
                return v;
            }
        }
    }
    None
}
fn parse_token(raw: &str) -> Option<Credential> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let get = |names: &[&str]| names.iter().find_map(|n| value.get(*n).and_then(Value::as_str));
    let mut credential = Credential {
        access_token: get(&["accessToken", "access_token", "token"]).map(SecretString::new),
        refresh_token: get(&["refreshToken", "refresh_token"]).map(SecretString::new),
        client_id: get(&["clientId", "client_id"]).map(SecretString::new),
        client_secret: get(&["clientSecret", "client_secret"]).map(SecretString::new),
        profile_arn: get(&["profileArn", "profile_arn"]).map(ToOwned::to_owned),
        sso_region: get(&["ssoRegion", "sso_region"]).map(ToOwned::to_owned),
        api_region: get(&["apiRegion", "api_region", "region"]).unwrap_or("us-east-1").to_owned(),
        endpoint: "cli".into(),
        machine_id: get(&["machineId", "machine_id"]).unwrap_or_default().to_owned(),
        expires_at: get(&["expiresAt", "expires_at"])
            .and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok())
            .map(|v| v.with_timezone(&chrono::Utc)),
        ..Default::default()
    };
    super::profile_resolver::resolve_profile(&value, &mut credential);
    Some(credential)
}
fn load_state_tables(conn: &Connection) -> Vec<Credential> {
    let mut out = Vec::new();
    for sql in ["SELECT value FROM state", "SELECT data FROM state"] {
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                for row in rows.flatten() {
                    if let Some(c) = parse_token(&row) {
                        out.push(c);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::load;
    use rusqlite::Connection;

    #[test]
    fn reads_auth_kv_and_profile_state_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("CREATE TABLE auth_kv(key TEXT PRIMARY KEY, value TEXT); CREATE TABLE state(key TEXT PRIMARY KEY, value TEXT);").unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv(key,value) VALUES('kirocli:odic:token', ?1)",
                [r#"{"accessToken":"a","refreshToken":"r","region":"us-west-2"}"#],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO state(key,value) VALUES('api.codewhisperer.profile', ?1)",
                [r#"{"arn":"arn:aws:codewhisperer:eu-west-1:123:profile/x"}"#],
            )
            .unwrap();
        drop(connection);
        let credentials = load(&path).unwrap();
        assert_eq!(
            credentials[0].profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:eu-west-1:123:profile/x")
        );
        assert_eq!(credentials[0].api_region, "eu-west-1");
    }
}
