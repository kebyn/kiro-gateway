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
    pub fn start(&mut self, id: Option<&str>, name: &str) -> Option<String> {
        let id = id.filter(|value| !value.is_empty());
        let name = (!name.is_empty()).then_some(name);
        let key = if let Some(real_id) = id {
            if self.buffers.contains_key(real_id) {
                real_id.to_owned()
            } else if let Some(generated) = self.rebind_generated(real_id, name) {
                generated
            } else {
                self.insert_buffer(real_id.to_owned(), name?, false)
            }
        } else {
            match self.last_open_tool() {
                Some(last)
                    if name.is_none()
                        || self
                            .buffers
                            .get(&last)
                            .is_some_and(|buffer| name.is_some_and(|name| buffer.name == name)) =>
                {
                    last
                }
                Some(last) => {
                    self.complete_buffer(&last, true);
                    self.insert_generated(name?)
                }
                None => self.insert_generated(name?),
            }
        };
        let buffer = self.buffers.get_mut(&key).expect("tool buffer must exist");
        if let Some(name) = name.filter(|_| buffer.name.is_empty()) {
            buffer.name = name.to_owned();
        }
        self.last_tool = Some(key.clone());
        Some(key)
    }

    fn insert_buffer(&mut self, key: String, name: &str, generated_id: bool) -> String {
        self.arrival_order.push(key.clone());
        self.buffers.insert(
            key.clone(),
            ToolBuffer {
                id: key.clone(),
                name: name.to_owned(),
                arguments: String::new(),
                complete: false,
                ended: false,
                generated_id,
            },
        );
        key
    }

    fn insert_generated(&mut self, name: &str) -> String {
        let key = format!("tool_call_{}", self.arrival_order.len() + 1);
        self.insert_buffer(key, name, true)
    }

    fn last_open_tool(&self) -> Option<String> {
        self.last_tool
            .clone()
            .filter(|key| self.buffers.get(key).is_some_and(|buffer| !buffer.ended))
    }

    fn rebind_generated(&mut self, real_id: &str, name: Option<&str>) -> Option<String> {
        let generated = self.last_open_tool()?;
        let can_rebind = self.buffers.get(&generated).is_some_and(|buffer| {
            buffer.generated_id && name.is_none_or(|name| buffer.name == name)
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

    pub fn append(&mut self, id: Option<&str>, fragment: &Value) -> Option<String> {
        let key = if let Some(id) = id.filter(|value| !value.is_empty()) {
            self.buffers.get(id).filter(|buffer| !buffer.ended).map(|_| id.to_owned())?
        } else {
            self.last_open_tool()?
        };
        let buffer = self.buffers.get_mut(&key).expect("tool buffer must exist");
        match fragment {
            Value::String(fragment) => buffer.arguments.push_str(fragment),
            Value::Object(object) if object.is_empty() => {}
            Value::Null => {}
            fragment => buffer.arguments = fragment.to_string(),
        }
        buffer.complete = false;
        self.last_tool = Some(key.clone());
        Some(key)
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
        self.complete_buffer(&key, complete);
        if self.last_tool.as_deref() == Some(&key) {
            self.last_tool = None;
        }
        let buffer = self.buffers.get(&key)?;
        Some(to_call(buffer))
    }

    fn complete_buffer(&mut self, key: &str, complete: bool) {
        if let Some(buffer) = self.buffers.get_mut(key) {
            buffer.complete = complete && arguments_are_complete(&buffer.arguments);
            buffer.ended = true;
        }
    }

    pub fn finish_all(&mut self) -> Vec<InternalToolCall> {
        for buffer in self.buffers.values_mut().filter(|buffer| !buffer.ended) {
            buffer.complete = arguments_are_complete(&buffer.arguments);
            buffer.ended = true;
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

fn arguments_are_complete(arguments: &str) -> bool {
    arguments.trim().is_empty()
        || serde_json::from_str::<Value>(arguments).is_ok_and(|value| value.is_object())
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
        state.append(None, &serde_json::json!("{\"query\":"));
        state.start(Some("call_real"), "search");
        state.append(Some("call_real"), &serde_json::json!("\"rust\"}"));
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
        state.append(Some("b"), &serde_json::json!("{}"));
        state.append(Some("a"), &serde_json::json!("{}"));
        let calls = state.finish_all();
        assert_eq!(calls.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn rebind_does_not_consume_the_next_parallel_tool() {
        let mut state = ToolCallAccumulator::new();
        state.start(None, "one");
        state.append(None, &serde_json::json!(r#"{"a":"#));
        state.start(Some("call_a"), "one");
        state.append(Some("call_a"), &serde_json::json!("1}"));
        state.start(Some("call_b"), "two");
        state.append(Some("call_b"), &serde_json::json!(r#"{"b":2}"#));
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
        state.append(Some("call"), &serde_json::json!("{\"a\":"));
        assert!(state.incomplete());
        assert!(!state.calls()[0].complete);
    }

    #[test]
    fn completes_valid_json_and_fills_late_name() {
        let mut state = ToolCallAccumulator::new();
        assert!(state.append(Some("call"), &serde_json::json!("{\"a\":")).is_none());
        state.start(Some("call"), "lookup");
        state.append(Some("call"), &serde_json::json!("{\"a\":1}"));
        let calls = state.finish_all();
        assert_eq!(calls[0].name, "lookup");
        assert!(calls[0].complete);
        assert_eq!(calls[0].arguments["a"], 1);
    }

    #[test]
    fn ignores_orphan_fragments_without_creating_unknown_tools() {
        let mut state = ToolCallAccumulator::new();
        assert!(state.append(Some("missing"), &serde_json::json!("{}")).is_none());
        assert!(state.append(None, &serde_json::json!("{}")).is_none());
        assert!(state.finish_all().is_empty());
    }

    #[test]
    fn idless_name_change_closes_old_tool_and_opens_new_one() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("first"), "alpha");
        state.append(Some("first"), &serde_json::json!(r#"{"a":1}"#));
        state.start(None, "beta");
        state.append(None, &serde_json::json!(r#"{"b":2}"#));
        let calls = state.finish_all();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "alpha");
        assert_eq!(calls[0].arguments["a"], 1);
        assert_eq!(calls[1].name, "beta");
        assert_eq!(calls[1].arguments["b"], 2);
    }

    #[test]
    fn stop_does_not_make_invalid_json_complete() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("call"), "lookup");
        state.append(Some("call"), &serde_json::json!("{\"query\":"));
        let call = state.finish(Some("call")).unwrap();
        assert!(!call.complete);
        assert_eq!(call.arguments, serde_json::json!("{\"query\":"));
    }

    #[test]
    fn object_input_replaces_fragments_and_empty_object_is_valid() {
        let mut state = ToolCallAccumulator::new();
        state.start(Some("object"), "lookup");
        state.append(Some("object"), &serde_json::json!({"query":"rust"}));
        state.start(Some("empty"), "noop");
        state.append(Some("empty"), &serde_json::json!({}));
        let calls = state.finish_all();
        assert_eq!(calls[0].arguments["query"], "rust");
        assert_eq!(calls[1].arguments, serde_json::json!({}));
        assert!(calls.iter().all(|call| call.complete));
    }
}
