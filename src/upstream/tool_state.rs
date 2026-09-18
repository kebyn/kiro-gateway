use serde_json::Value;
use std::collections::HashMap;

use crate::protocol::internal::InternalToolCall;

#[derive(Clone, Debug)]
struct ToolBuffer {
    id: String,
    name: String,
    arguments: String,
    complete: bool,
    ended: bool,
    generated_id: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ToolCallAccumulator {
    buffers: HashMap<String, ToolBuffer>,
    arrival_order: Vec<String>,
    last_tool: Option<String>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn start(&mut self, id: Option<&str>, name: &str) -> String {
        let key = if let Some(real_id) = id.filter(|value| !value.is_empty()) {
            if self.buffers.contains_key(real_id) {
                real_id.to_owned()
            } else if let Some(generated) = self.rebind_generated(real_id, name) {
                generated
            } else {
                self.insert_buffer(real_id.to_owned(), name, false)
            }
        } else if let Some(last) = self.last_tool.clone().filter(|key| {
            self.buffers.get(key).is_some_and(|buffer| {
                !buffer.ended
                    && (name.is_empty() || buffer.name == name || buffer.name == "unknown")
            })
        }) {
            last
        } else {
            let generated = format!("tool_call_{}", self.arrival_order.len() + 1);
            self.insert_buffer(generated, name, true)
        };
        let buffer = self.buffers.get_mut(&key).expect("tool buffer must exist");
        if !name.is_empty() && buffer.name == "unknown" {
            buffer.name = name.to_owned();
        }
        self.last_tool = Some(key.clone());
        key
    }

    fn insert_buffer(&mut self, key: String, name: &str, generated_id: bool) -> String {
        self.arrival_order.push(key.clone());
        self.buffers.insert(
            key.clone(),
            ToolBuffer {
                id: key.clone(),
                name: if name.is_empty() { "unknown".into() } else { name.to_owned() },
                arguments: String::new(),
                complete: false,
                ended: false,
                generated_id,
            },
        );
        key
    }

    fn rebind_generated(&mut self, real_id: &str, name: &str) -> Option<String> {
        let generated = self.last_tool.clone()?;
        let can_rebind = self.buffers.get(&generated).is_some_and(|buffer| {
            buffer.generated_id
                && !buffer.ended
                && (name.is_empty() || buffer.name == name || buffer.name == "unknown")
        });
        if !can_rebind {
            return None;
        }
        let mut buffer = self.buffers.remove(&generated)?;
        buffer.id = real_id.to_owned();
        buffer.generated_id = false;
        self.buffers.insert(real_id.to_owned(), buffer);
        if let Some(position) = self.arrival_order.iter().position(|key| key == &generated) {
            self.arrival_order[position] = real_id.to_owned();
        }
        Some(real_id.to_owned())
    }

    pub fn append(&mut self, id: Option<&str>, fragment: &str) -> String {
        let key = if let Some(id) = id.filter(|value| !value.is_empty()) {
            if self.buffers.contains_key(id) {
                id.to_owned()
            } else {
                self.insert_buffer(id.to_owned(), "unknown", false)
            }
        } else {
            self.last_tool.clone().unwrap_or_else(|| self.start(None, "unknown"))
        };
        let buffer = self.buffers.get_mut(&key).expect("tool buffer must exist");
        buffer.arguments.push_str(fragment);
        self.last_tool = Some(key.clone());
        key
    }
    pub fn rename(&mut self, id: &str, name: &str) {
        if let Some(buffer) = self.buffers.get_mut(id) {
            buffer.name = name.to_owned();
        }
    }
    pub fn finish(&mut self, id: Option<&str>) -> Option<InternalToolCall> {
        self.finish_with_state(id, true)
    }
    pub fn finish_with_state(
        &mut self,
        id: Option<&str>,
        complete: bool,
    ) -> Option<InternalToolCall> {
        let key = id
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| self.last_tool.clone())?;
        let buffer = self.buffers.get_mut(&key)?;
        buffer.complete = complete;
        buffer.ended = true;
        if self.last_tool.as_deref() == Some(&key) {
            self.last_tool = None;
        }
        Some(to_call(buffer))
    }
    pub fn finish_all(&mut self) -> Vec<InternalToolCall> {
        for buffer in self.buffers.values_mut().filter(|buffer| !buffer.ended) {
            buffer.complete = buffer.arguments.trim().is_empty()
                || serde_json::from_str::<Value>(&buffer.arguments).is_ok();
        }
        self.arrival_order.iter().filter_map(|key| self.buffers.get(key).map(to_call)).collect()
    }
    pub fn incomplete(&self) -> bool {
        self.buffers.values().any(|b| !b.complete)
    }
    pub fn calls(&self) -> Vec<InternalToolCall> {
        self.arrival_order.iter().filter_map(|key| self.buffers.get(key).map(to_call)).collect()
    }
}

fn to_call(buffer: &ToolBuffer) -> InternalToolCall {
    let arguments = if buffer.arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str::<Value>(&buffer.arguments)
            .unwrap_or_else(|_| Value::String(buffer.arguments.clone()))
    };
    InternalToolCall {
        id: buffer.id.clone(),
        name: buffer.name.clone(),
        arguments,
        complete: buffer.complete,
    }
}

#[cfg(test)]
mod tests {
    use super::ToolCallAccumulator;

    #[test]
    fn rebinds_idless_tool_to_real_id() {
        let mut state = ToolCallAccumulator::new();
        state.start(None, "search");
        state.append(None, "{\"query\":");
        state.start(Some("call_real"), "search");
        state.append(Some("call_real"), "\"rust\"}");
        let calls = state.finish_all();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_real");
        assert_eq!(calls[0].arguments["query"], "rust");
    }

    #[test]
    fn preserves_interleaved_tool_order() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("a"), "one");
        state.start(Some("b"), "two");
        state.append(Some("b"), "{}");
        state.append(Some("a"), "{}");
        let calls = state.finish_all();
        assert_eq!(calls.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn rebind_does_not_consume_the_next_parallel_tool() {
        let mut state = ToolCallAccumulator::new();
        state.start(None, "one");
        state.append(None, r#"{"a":"#);
        state.start(Some("call_a"), "one");
        state.append(Some("call_a"), "1}");
        state.start(Some("call_b"), "two");
        state.append(Some("call_b"), r#"{"b":2}"#);
        let calls = state.finish_all();
        assert_eq!(
            calls.iter().map(|call| call.id.as_str()).collect::<Vec<_>>(),
            ["call_a", "call_b"]
        );
        assert_eq!(calls[0].arguments["a"], 1);
        assert_eq!(calls[1].arguments["b"], 2);
    }

    #[test]
    fn treats_empty_arguments_as_an_empty_object() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("call"), "no_args");
        let calls = state.finish_all();
        assert!(calls[0].complete);
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn marks_unfinished_arguments_incomplete() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("call"), "tool");
        state.append(Some("call"), "{\"a\":");
        assert!(state.incomplete());
        assert!(!state.calls()[0].complete);
    }

    #[test]
    fn completes_valid_json_and_fills_late_name() {
        let mut state = ToolCallAccumulator::new();
        state.append(Some("call"), "{\"a\":");
        state.start(Some("call"), "lookup");
        state.append(Some("call"), "1}");
        let calls = state.finish_all();
        assert_eq!(calls[0].name, "lookup");
        assert!(calls[0].complete);
        assert_eq!(calls[0].arguments["a"], 1);
    }
}
