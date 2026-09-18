use thiserror::Error;

#[derive(Debug, Error)]
pub enum UpstreamStreamError {
    #[error("truncated event-stream frame: expected {expected} bytes, received {received}")]
    TruncatedFrame { expected: usize, received: usize },
    #[error("invalid event-stream frame length {0}")]
    InvalidLength(u32),
    #[error("event-stream CRC mismatch")]
    CrcMismatch,
    #[error("malformed event-stream header")]
    MalformedHeader,
    #[error("upstream event error: {0}")]
    Event(String),
    #[error("upstream exception: {0}")]
    Upstream(String),
}
