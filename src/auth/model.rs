use std::{fmt, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}
impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("[REDACTED]")
    }
}
impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(String::deserialize(deserializer)?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    ApiKey,
    Social,
    Sso,
    RefreshToken,
    Unknown,
}

impl Default for AuthMethod {
    fn default() -> Self {
        Self::Unknown
    }
}
impl FromStr for AuthMethod {
    type Err = ();
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value.to_ascii_lowercase().as_str() {
            "api_key" | "apikey" | "api-key" => Self::ApiKey,
            "social" => Self::Social,
            "sso" | "odic" => Self::Sso,
            "refresh_token" | "refresh-token" => Self::RefreshToken,
            _ => Self::Unknown,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Credential {
    pub auth_method: AuthMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_token: Option<SecretString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<SecretString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<SecretString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<SecretString>,
    pub profile_arn: Option<String>,
    pub sso_region: Option<String>,
    pub api_region: String,
    pub endpoint: String,
    pub machine_id: String,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub source: Option<String>,
}

impl Default for Credential {
    fn default() -> Self {
        Self {
            auth_method: AuthMethod::Unknown,
            access_token: None,
            refresh_token: None,
            client_id: None,
            client_secret: None,
            profile_arn: None,
            sso_region: None,
            api_region: "us-east-1".into(),
            endpoint: "ide".into(),
            machine_id: String::new(),
            expires_at: None,
            source: None,
        }
    }
}

impl Credential {
    pub fn usable(&self) -> bool {
        matches!(self.auth_method, AuthMethod::ApiKey)
            || self.access_token.as_ref().is_some_and(|v| !v.is_empty())
            || self.refresh_token.as_ref().is_some_and(|v| !v.is_empty())
    }
    pub fn needs_refresh(&self, now: DateTime<Utc>, early_secs: i64) -> bool {
        if matches!(self.auth_method, AuthMethod::ApiKey) {
            return false;
        }
        let refresh_at =
            now.checked_add_signed(chrono::Duration::seconds(early_secs)).unwrap_or(now);
        self.access_token.as_ref().is_none_or(|v| v.is_empty())
            || self.expires_at.is_some_and(|when| when <= refresh_at)
    }
    pub fn status(&self, now: DateTime<Utc>, early_secs: i64) -> CredentialStatus {
        CredentialStatus {
            auth_method: self.auth_method,
            has_access_token: self.access_token.as_ref().is_some_and(|v| !v.is_empty()),
            has_refresh_token: self.refresh_token.as_ref().is_some_and(|v| !v.is_empty()),
            expires_at: self.expires_at,
            needs_refresh: self.needs_refresh(now, early_secs),
            api_region: self.api_region.clone(),
            sso_region: self.sso_region.clone(),
            endpoint: self.endpoint.clone(),
            source: self.source.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthMethod, Credential, SecretString};
    use chrono::{Duration, Utc};

    #[test]
    fn secret_debug_and_serialization_are_redacted() {
        let secret = SecretString::new("very-secret-token");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(serde_json::to_string(&secret).unwrap(), "\"[REDACTED]\"");
    }

    #[test]
    fn refresh_window_is_applied() {
        let credential = Credential {
            auth_method: AuthMethod::RefreshToken,
            access_token: Some(SecretString::new("a")),
            expires_at: Some(Utc::now() + Duration::seconds(60)),
            ..Default::default()
        };
        assert!(credential.needs_refresh(Utc::now(), 120));
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CredentialStatus {
    pub auth_method: AuthMethod,
    pub has_access_token: bool,
    pub has_refresh_token: bool,
    pub expires_at: Option<DateTime<Utc>>,
    pub needs_refresh: bool,
    pub api_region: String,
    pub sso_region: Option<String>,
    pub endpoint: String,
    pub source: Option<String>,
}
