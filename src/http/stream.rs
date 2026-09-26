//! Shared internal stream driver. Protocol handlers are responsible only for
//! naming and shaping lifecycle events; accumulation, EOF/error handling and
//! the single terminal transition live here.

use crate::{
    error::AppError,
    generation::{GenerationEvent, GenerationResult},
    upstream::GenerationAccumulator,
};
use futures_util::StreamExt;

pub struct GenerationStreamDriver {
    accumulator: GenerationAccumulator,
    terminal_emitted: bool,
}

impl Default for GenerationStreamDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl GenerationStreamDriver {
    pub fn new() -> Self {
        Self { accumulator: GenerationAccumulator::new(), terminal_emitted: false }
    }

    pub fn push(&mut self, event: GenerationEvent) -> Result<(), AppError> {
        self.accumulator.push(event)
    }

    pub fn finish(mut self) -> GenerationResult {
        self.terminal_emitted = true;
        self.accumulator.finish()
    }
}

/// Consumes an internal stream through one accumulator path. The callback is
/// invoked before the event is accumulated, allowing a protocol adapter to
/// emit deltas while retaining one consistent error/EOF policy.
pub async fn drive<S, F>(mut stream: S, mut on_event: F) -> Result<GenerationResult, AppError>
where
    S: futures_core::Stream<Item = Result<GenerationEvent, AppError>> + Unpin,
    F: FnMut(&GenerationEvent) -> Result<(), AppError>,
{
    let mut driver = GenerationStreamDriver::new();
    while let Some(item) = stream.next().await {
        let event = item?;
        on_event(&event)?;
        driver.push(event)?;
    }
    Ok(driver.finish())
}
