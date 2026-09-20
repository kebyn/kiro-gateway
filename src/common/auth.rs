use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn constant_time_eq(left: &str, right: &str) -> bool {
    // Hashing first gives both inputs the same fixed-size representation. The
    // comparison therefore does not short-circuit on either length or the
    // first differing byte.
    let left_digest = Sha256::digest(left.as_bytes());
    let right_digest = Sha256::digest(right.as_bytes());
    left_digest.as_slice().ct_eq(right_digest.as_slice()).into()
}

pub fn extract_api_key(headers: &http::HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !value.is_empty() {
            return Some(value.to_owned());
        }
    }
    headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

pub fn is_safe_method(method: &http::Method) -> bool {
    matches!(*method, http::Method::GET | http::Method::HEAD | http::Method::OPTIONS)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_bearer_and_x_api_key() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(extract_api_key(&headers).as_deref(), Some("abc"));
        headers.insert("x-api-key", "xyz".parse().unwrap());
        assert_eq!(extract_api_key(&headers).as_deref(), Some("xyz"));
        assert!(constant_time_eq("same", "same"));
        assert!(!constant_time_eq("same", "other"));
        assert!(!constant_time_eq("", "x"));
        assert!(!constant_time_eq("short", &"x".repeat(100_000)));
        assert!(constant_time_eq("x".repeat(100_000).as_str(), "x".repeat(100_000).as_str()));
    }
}
