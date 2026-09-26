use crate::{
    error::AppError,
    generation::{GenerationEvent, GenerationResult, StopReason},
    transform::truncation::XmlLeakFilter,
    upstream::{integrity::StreamIntegrity, tool_state::ToolCallAccumulator},
};

/// Shared state machine used by complete responses and by all streaming HTTP
/// handlers. It preserves tool ordering and the cross-chunk XML filter.
pub struct GenerationAccumulator {
    output: GenerationResult,
    tools: ToolCallAccumulator,
    xml_filter: XmlLeakFilter,
    integrity: StreamIntegrity,
}

impl Default for GenerationAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl GenerationAccumulator {
    pub fn new() -> Self {
        Self {
            output: GenerationResult::default(),
            tools: ToolCallAccumulator::new(),
            xml_filter: XmlLeakFilter::new(),
            integrity: StreamIntegrity::default(),
        }
    }

    pub fn push(&mut self, event: GenerationEvent) -> Result<(), AppError> {
        apply_event(
            event,
            &mut self.output,
            &mut self.tools,
            &mut self.xml_filter,
            &mut self.integrity,
        )
    }

    pub fn finish(mut self) -> GenerationResult {
        let tail = self.xml_filter.finish();
        self.output.text.push_str(&tail);
        finish_result(self.output, &mut self.tools)
    }
}

pub(crate) fn apply_event(
    event: GenerationEvent,
    output: &mut GenerationResult,
    tools: &mut ToolCallAccumulator,
    xml_filter: &mut XmlLeakFilter,
    integrity: &mut StreamIntegrity,
) -> Result<(), AppError> {
    match event {
        GenerationEvent::TextDelta { text } => {
            integrity.record_emission();
            output.text.push_str(&xml_filter.push(&text));
        }
        GenerationEvent::ThinkingDelta { text } => {
            integrity.record_emission();
            output.thinking.push_str(&text);
        }
        GenerationEvent::ToolCallStart { id, name } => {
            tools.start(Some(&id), &name);
        }
        GenerationEvent::ToolCallDelta { id, arguments, name } => {
            if let Some(name) = name {
                tools.start(Some(&id), &name);
            }
            tools.append(Some(&id), &arguments);
        }
        GenerationEvent::ToolCallEnd { id, complete } => {
            if tools.finish_with_state(Some(&id), complete).is_some_and(|call| call.complete) {
                integrity.completed = true;
            }
        }
        GenerationEvent::Usage { usage } => output.usage = Some(usage),
        GenerationEvent::Stop { reason } => {
            integrity.completed = true;
            output.stop_reason = Some(reason);
        }
        GenerationEvent::Error { message } => return Err(AppError::Upstream(message)),
    }
    Ok(())
}

pub(crate) fn finish_result(
    mut output: GenerationResult,
    tools: &mut ToolCallAccumulator,
) -> GenerationResult {
    output.tool_calls = tools.finish_all();
    let truncated_without_terminal = output.stop_reason.is_none()
        && output.tool_calls.is_empty()
        && (!output.text.is_empty() || !output.thinking.is_empty());
    let stopped_incomplete = output.stop_reason.as_ref().is_some_and(|reason| {
        matches!(
            reason,
            StopReason::MaxTokens
                | StopReason::ContextWindowExceeded
                | StopReason::Refusal
                | StopReason::StreamIncomplete
        )
    });
    output.incomplete = output.tool_calls.iter().any(|call| !call.complete)
        || truncated_without_terminal
        || stopped_incomplete;
    output
}
