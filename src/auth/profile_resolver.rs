use serde_json::Value;

use super::Credential;

/// Resolves profile ARN and API region from the nested CLI state object.
pub fn resolve_profile(value: &Value, credential: &mut Credential) {
    let profile = value.get("arn").map(|_| value).or_else(|| {
        value
            .pointer("/state/api/codewhisperer/profile")
            .or_else(|| value.pointer("/api/codewhisperer/profile"))
    });
    if let Some(profile) = profile {
        credential.profile_arn = profile
            .get("arn")
            .or_else(|| profile.get("profileArn"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| profile.as_str().map(ToOwned::to_owned));
        if let Some(region) =
            profile.get("region").or_else(|| profile.get("apiRegion")).and_then(Value::as_str)
        {
            credential.api_region = region.to_owned();
        } else if let Some(arn) = credential.profile_arn.as_deref() {
            let parts: Vec<&str> = arn.split(':').collect();
            if parts.len() > 3 && !parts[3].is_empty() {
                credential.api_region = parts[3].to_owned();
            }
        }
    }
}
