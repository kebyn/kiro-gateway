#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryDecision {
    Retry,
    DoNotRetry,
}

#[derive(Clone, Debug, Default)]
pub struct StreamIntegrity {
    pub emitted_any: bool,
    pub completed: bool,
    pub incomplete: bool,
    pub attempts: u8,
}
impl StreamIntegrity {
    pub fn record_emission(&mut self) {
        self.emitted_any = true;
    }
    pub fn should_retry(&self) -> RetryDecision {
        if !self.emitted_any && self.incomplete && self.attempts < 1 {
            RetryDecision::Retry
        } else {
            RetryDecision::DoNotRetry
        }
    }
}
