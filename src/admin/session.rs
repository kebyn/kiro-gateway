use parking_lot::RwLock;
use rand::{Rng, distr::Alphanumeric};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct Session {
    pub token: String,
    pub csrf_token: String,
    pub expires_at: Instant,
}

#[derive(Clone, Default)]
pub struct SessionStore {
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    attempts: Arc<RwLock<HashMap<String, (Instant, u32)>>>,
}
impl SessionStore {
    pub fn create(&self, ttl: Duration) -> Session {
        let mut rng = rand::rng();
        let token: String =
            (&mut rng).sample_iter(&Alphanumeric).take(48).map(char::from).collect();
        let csrf_token: String =
            (&mut rng).sample_iter(&Alphanumeric).take(32).map(char::from).collect();
        let session =
            Session { token: token.clone(), csrf_token, expires_at: Instant::now() + ttl };
        self.sessions.write().insert(token, session.clone());
        session
    }
    pub fn get(&self, token: &str) -> Option<Session> {
        let now = Instant::now();
        let mut sessions = self.sessions.write();
        let session = sessions.get(token).cloned();
        match session {
            Some(session) if session.expires_at > now => Some(session),
            Some(_) => {
                sessions.remove(token);
                None
            }
            None => None,
        }
    }
    pub fn remove(&self, token: &str) {
        self.sessions.write().remove(token);
    }
    pub fn allow_login(&self, key: &str, limit: u32) -> bool {
        let now = Instant::now();
        let mut attempts = self.attempts.write();
        attempts.retain(|_, (started, _)| now.duration_since(*started) < Duration::from_secs(60));
        let entry = attempts.entry(key.to_owned()).or_insert((now, 0));
        if now.duration_since(entry.0) >= Duration::from_secs(60) {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            return false;
        }
        entry.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::SessionStore;
    use std::time::Duration;

    #[test]
    fn expired_sessions_are_removed_on_lookup() {
        let store = SessionStore::default();
        let session = store.create(Duration::ZERO);
        assert!(store.get(&session.token).is_none());
        assert!(store.get(&session.token).is_none());
    }

    #[test]
    fn login_attempts_are_limited_per_key() {
        let store = SessionStore::default();
        assert!(store.allow_login("global", 1));
        assert!(!store.allow_login("global", 1));
        assert!(store.allow_login("other", 1));
    }
}
