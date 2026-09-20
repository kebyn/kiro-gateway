use super::{AuthMethod, Credential, SecretString};
use crate::error::AppError;
use rusqlite::{Connection, OpenFlags, OptionalExtension, types::ValueRef};
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
                    if key.contains("social") { AuthMethod::Social } else { AuthMethod::Oidc };
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
        if let Ok(Some(profile)) = conn.query_row(
            "SELECT value FROM state WHERE key = 'api.codewhisperer.profile'",
            [],
            |row| read_text_value(row, 0),
        ) {
            if let Ok(value) = serde_json::from_str::<Value>(&profile) {
                super::profile_resolver::resolve_profile(&value, first);
            }
        }
    }
    if result.is_empty() {
        return Err(AppError::Credential("no supported Kiro token found in SQLite".into()));
    }
    if result.len() != 1 {
        return Err(AppError::Credential(
            "multiple credentials discovered in SQLite; expected exactly one".into(),
        ));
    }
    result[0].validate_external().map_err(AppError::Credential)?;
    Ok(result)
}

fn read_any(conn: &Connection, key: &str) -> Option<String> {
    for sql in [
        "SELECT value FROM auth_kv WHERE key = ?1",
        "SELECT value FROM key_value WHERE key = ?1",
        "SELECT value FROM settings WHERE key = ?1",
        "SELECT data FROM state WHERE key = ?1",
    ] {
        if let Ok(Some(Some(value))) =
            conn.query_row(sql, [key], |row| read_text_value(row, 0)).optional()
        {
            return Some(value);
        }
    }
    None
}

fn read_text_value(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(index)? {
        ValueRef::Text(value) => Some(String::from_utf8_lossy(value).into_owned()),
        ValueRef::Blob(value) => String::from_utf8(value.to_vec()).ok(),
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => None,
    })
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
            if let Ok(rows) = stmt.query_map([], |row| read_text_value(row, 0)) {
                for raw in rows.flatten().flatten() {
                    if let Some(c) = parse_token(&raw) {
                        let mut credential = c;
                        credential.auth_method = AuthMethod::Oidc;
                        out.push(credential);
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{AuthMethod, load};
    use rusqlite::Connection;
    use std::{
        env, fs,
        path::{Path, PathBuf},
    };

    fn create_realistic_database(path: &Path) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE auth_kv (
                    key TEXT PRIMARY KEY,
                    value TEXT
                );
                CREATE TABLE state (
                    key TEXT PRIMARY KEY,
                    value BLOB
                );
                CREATE TABLE conversations (
                    key TEXT PRIMARY KEY,
                    value TEXT
                );
                CREATE TABLE conversations_v2 (
                    key TEXT NOT NULL,
                    conversation_id TEXT NOT NULL,
                    value TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    PRIMARY KEY (key, conversation_id)
                );
                "#,
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv(key,value) VALUES('kirocli:odic:token', ?1)",
                [r#"{"access_token":"access-token-fixture","refresh_token":"refresh-token-fixture","region":"us-west-2","expires_at":"2030-01-02T03:04:05Z"}"#],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO auth_kv(key,value) VALUES('kirocli:odic:device-registration', ?1)",
                [r#"{"client_id":"client-id-fixture","client_secret":"client-secret-fixture","region":"us-east-2"}"#],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO state(key,value) VALUES('api.codewhisperer.profile', ?1)",
                [r#"{"arn":"arn:aws:codewhisperer:eu-west-1:123:profile/fixture","profile_name":"fixture"}"#.as_bytes()],
            )
            .unwrap();
    }

    #[test]
    fn reads_realistic_auth_kv_and_blob_profile_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.sqlite3");
        create_realistic_database(&path);
        let before = fs::metadata(&path).unwrap();

        let credentials = load(&path).unwrap();

        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().ok(), after.modified().ok());
        assert_eq!(credentials.len(), 1);
        let credential = &credentials[0];
        assert_eq!(credential.auth_method, AuthMethod::Oidc);
        assert_eq!(
            credential.access_token.as_ref().map(|value| value.expose_secret()),
            Some("access-token-fixture")
        );
        assert_eq!(
            credential.refresh_token.as_ref().map(|value| value.expose_secret()),
            Some("refresh-token-fixture")
        );
        assert_eq!(
            credential.client_id.as_ref().map(|value| value.expose_secret()),
            Some("client-id-fixture")
        );
        assert_eq!(
            credential.client_secret.as_ref().map(|value| value.expose_secret()),
            Some("client-secret-fixture")
        );
        assert_eq!(
            credential.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:eu-west-1:123:profile/fixture")
        );
        assert_eq!(credential.api_region, "eu-west-1");
        assert_eq!(credential.sso_region.as_deref(), Some("us-east-2"));
    }

    #[test]
    #[ignore = "requires a local Kiro CLI SQLite database"]
    fn reads_real_kiro_cli_database_read_only() {
        let path = env::var_os("KIRO_REAL_SQLITE_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".local/share/kiro-cli/data.sqlite3"))
            })
            .expect("HOME or KIRO_REAL_SQLITE_PATH must be set");
        assert!(path.is_file(), "SQLite database does not exist");
        let before = fs::metadata(&path).unwrap();

        let credentials = load(&path).expect("real Kiro CLI SQLite database should be readable");

        let after = fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().ok(), after.modified().ok());
        assert!(!credentials.is_empty());
        let credential = &credentials[0];
        assert!(matches!(credential.auth_method, AuthMethod::Social | AuthMethod::Oidc));
        assert!(credential.access_token.as_ref().is_some_and(|value| !value.is_empty()));
        assert!(credential.refresh_token.as_ref().is_some_and(|value| !value.is_empty()));
        assert!(credential.client_id.as_ref().is_some_and(|value| !value.is_empty()));
        assert!(credential.client_secret.as_ref().is_some_and(|value| !value.is_empty()));
        assert!(credential.profile_arn.as_deref().is_some_and(|value| !value.is_empty()));
        assert!(!credential.api_region.is_empty());
        assert_eq!(credential.endpoint, "cli");
    }
}
