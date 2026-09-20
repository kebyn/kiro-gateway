//! Shared internal stream driver. Protocol handlers are responsible only for
//! naming and shaping lifecycle events; accumulation, EOF/error handling and
//! the single terminal transition live here.

use crate::{
    error::AppError,
    protocol::internal::{InternalEvent, InternalResponse},
    upstream::request::InternalEventAccumulator,
};
use futures_util::StreamExt;

pub struct InternalStreamDriver {
    accumulator: InternalEventAccumulator,
    terminal_emitted: bool,
}

impl Default for InternalStreamDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl InternalStreamDriver {
    pub fn new() -> Self {
        Self { accumulator: InternalEventAccumulator::new(), terminal_emitted: false }
    }

    pub fn push(&mut self, event: InternalEvent) -> Result<(), AppError> {
        self.accumulator.push(event)
    }

    pub fn finish(mut self) -> InternalResponse {
        self.terminal_emitted = true;
        self.accumulator.finish()
    }
}

/// Consumes an internal stream through one accumulator path. The callback is
/// invoked before the event is accumulated, allowing a protocol adapter to
/// emit deltas while retaining one consistent error/EOF policy.
pub async fn drive<S, F>(mut stream: S, mut on_event: F) -> Result<InternalResponse, AppError>
where
    S: futures_core::Stream<Item = Result<InternalEvent, AppError>> + Unpin,
    F: FnMut(&InternalEvent) -> Result<(), AppError>,
{
    let mut driver = InternalStreamDriver::new();
    while let Some(item) = stream.next().await {
        let event = item?;
        on_event(&event)?;
        driver.push(event)?;
    }
    Ok(driver.finish())
}
