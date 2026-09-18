use serde_json::Value;
use std::collections::HashMap;

use crate::protocol::internal::InternalToolCall;

#[derive(Clone, Debug)]
struct ToolBuffer {
    id: String,
    name: String,
    arguments: String,
    complete: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ToolCallAccumulator {
    buffers: HashMap<String, ToolBuffer>,
    arrival_order: Vec<String>,
    pub active_idless_tool: Option<String>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn start(&mut self, id: Option<&str>, name: &str) -> String {
        let requested = id
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("tool_call_{}", self.arrival_order.len() + 1));
        // Kiro may begin a tool without an ID and attach the real ID on a later
        // continuation. Rebind the synthetic key instead of emitting a second call.
        let key = if let Some(real_id) = id.filter(|v| !v.is_empty()) {
            if let Some(synthetic) = self.active_idless_tool.clone() {
                if synthetic != real_id && self.buffers.get(&synthetic).is_some_and(|b| !b.complete)
                {
                    if let Some(mut buffer) = self.buffers.remove(&synthetic) {
                        buffer.id = real_id.to_owned();
                        self.buffers.insert(real_id.to_owned(), buffer);
                        if let Some(position) =
                            self.arrival_order.iter().position(|v| v == &synthetic)
                        {
                            self.arrival_order[position] = real_id.to_owned();
                        }
                    }
                    self.active_idless_tool = Some(real_id.to_owned());
                }
            }
            real_id.to_owned()
        } else {
            requested
        };
        if !self.buffers.contains_key(&key) {
            self.arrival_order.push(key.clone());
        }
        self.buffers.entry(key.clone()).or_insert_with(|| ToolBuffer {
            id: key.clone(),
            name: name.to_owned(),
            arguments: String::new(),
            complete: false,
        });
        self.active_idless_tool =
            if id.is_none() { Some(key.clone()) } else { self.active_idless_tool.clone() };
        key
    }
    pub fn append(&mut self, id: Option<&str>, fragment: &str) -> String {
        let key = id
            .filter(|v| !v.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| self.active_idless_tool.clone())
            .unwrap_or_else(|| self.start(None, "unknown"));
        let buffer = self.buffers.entry(key.clone()).or_insert_with(|| {
            self.arrival_order.push(key.clone());
            ToolBuffer {
                id: key.clone(),
                name: "unknown".into(),
                arguments: String::new(),
                complete: false,
            }
        });
        buffer.arguments.push_str(fragment);
        key
    }
    pub fn rename(&mut self, id: &str, name: &str) {
        if let Some(buffer) = self.buffers.get_mut(id) {
            buffer.name = name.to_owned();
        }
    }
    pub fn finish(&mut self, id: Option<&str>) -> Option<InternalToolCall> {
        let key = id.map(ToOwned::to_owned).or_else(|| self.active_idless_tool.clone())?;
        let buffer = self.buffers.get_mut(&key)?;
        buffer.complete = true;
        Some(to_call(buffer))
    }
    pub fn finish_all(&mut self) -> Vec<InternalToolCall> {
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
    InternalToolCall {
        id: buffer.id.clone(),
        name: buffer.name.clone(),
        arguments: serde_json::from_str::<Value>(&buffer.arguments)
            .unwrap_or_else(|_| Value::String(buffer.arguments.clone())),
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
    fn marks_unfinished_arguments_incomplete() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("call"), "tool");
        state.append(Some("call"), "{\"a\":");
        assert!(state.incomplete());
        assert!(!state.calls()[0].complete);
    }
}
