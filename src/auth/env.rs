use super::{AuthMethod, Credential, SecretString};

pub fn load() -> Vec<Credential> {
    let access = std::env::var("KIRO_ACCESS_TOKEN").ok().filter(|v| !v.is_empty());
    let refresh = std::env::var("KIRO_REFRESH_TOKEN").ok().filter(|v| !v.is_empty());
    let api_key = std::env::var("KIRO_API_KEY").ok().filter(|v| !v.is_empty());
    if access.is_none() && refresh.is_none() && api_key.is_none() {
        return Vec::new();
    }
    vec![Credential {
        auth_method: if api_key.is_some() { AuthMethod::ApiKey } else { AuthMethod::RefreshToken },
        access_token: access.map(SecretString::new).or_else(|| api_key.map(SecretString::new)),
        refresh_token: refresh.map(SecretString::new),
        client_id: std::env::var("KIRO_CLIENT_ID").ok().map(SecretString::new),
        client_secret: std::env::var("KIRO_CLIENT_SECRET").ok().map(SecretString::new),
        api_region: std::env::var("KIRO_API_REGION").unwrap_or_else(|_| "us-east-1".into()),
        endpoint: std::env::var("KIRO_ENDPOINT").unwrap_or_else(|_| "auto".into()),
        machine_id: std::env::var("KIRO_MACHINE_ID")
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
        ..Default::default()
    }]
}
