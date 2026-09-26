use std::collections::HashMap;

use serde_json::Value;

use crate::generation::{GenerationResult, StopReason, ToolCall, Usage};

#[derive(Clone, Debug)]
pub(super) struct LiveTool {
    pub(super) call_id: String,
    pub(super) item_id: String,
    pub(super) name: String,
    pub(super) arguments: String,
    pub(super) output_index: usize,
    pub(super) ended: bool,
    pub(super) done_emitted: bool,
    pub(super) custom: bool,
    pub(super) response_name: String,
    pub(super) namespace: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct LiveReasoning {
    pub(super) item_id: String,
    pub(super) output_index: usize,
    pub(super) summary: String,
    pub(super) done_emitted: bool,
}

#[derive(Clone, Debug)]
pub(super) struct CustomToolInfo {
    pub(super) name: String,
    pub(super) namespace: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) enum LiveItem {
    Reasoning(usize),
    Text,
    Tool(usize),
}

#[derive(Default)]
pub(super) struct LiveResponseState {
    pub(super) text_item_id: Option<String>,
    pub(super) text_output_index: Option<usize>,
    pub(super) reasoning: Vec<LiveReasoning>,
    pub(super) active_reasoning: Option<usize>,
    pub(super) tools: Vec<LiveTool>,
    pub(super) tool_indices: HashMap<String, usize>,
    pub(super) item_order: Vec<LiveItem>,
    pub(super) next_output_index: usize,
    pub(super) text: String,
    pub(super) thinking: String,
    pub(super) stop_reason: Option<StopReason>,
    pub(super) usage: Option<Usage>,
}

impl LiveResponseState {
    pub(super) fn snapshot_response(&self) -> GenerationResult {
        GenerationResult {
            text: self.text.clone(),
            thinking: self.thinking.clone(),
            tool_calls: self
                .tools
                .iter()
                .map(|tool| ToolCall {
                    id: tool.call_id.clone(),
                    name: tool.name.clone(),
                    arguments: serde_json::from_str(&tool.arguments)
                        .unwrap_or_else(|_| Value::String(tool.arguments.clone())),
                    complete: tool.ended
                        && (tool.arguments.trim().is_empty()
                            || serde_json::from_str::<Value>(&tool.arguments)
                                .is_ok_and(|value| value.is_object())),
                })
                .collect(),
            usage: self.usage.clone(),
            stop_reason: self.stop_reason.clone(),
            incomplete: self.stop_reason.is_none(),
        }
    }

    pub(super) fn ensure_text(&mut self) -> (String, usize, bool) {
        if let (Some(id), Some(index)) = (&self.text_item_id, self.text_output_index) {
            return (id.clone(), index, false);
        }
        let id = format!("msg_{}", uuid::Uuid::now_v7());
        let index = self.next_output_index;
        self.next_output_index += 1;
        self.text_item_id = Some(id.clone());
        self.text_output_index = Some(index);
        self.item_order.push(LiveItem::Text);
        (id, index, true)
    }

    pub(super) fn ensure_reasoning(&mut self) -> (usize, bool) {
        if let Some(index) = self.active_reasoning {
            return (index, false);
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let index = self.reasoning.len();
        self.reasoning.push(LiveReasoning {
            item_id: format!("rs_{}", uuid::Uuid::now_v7()),
            output_index,
            summary: String::new(),
            done_emitted: false,
        });
        self.active_reasoning = Some(index);
        self.item_order.push(LiveItem::Reasoning(index));
        (index, true)
    }

    pub(super) fn ensure_tool(
        &mut self,
        call_id: &str,
        name: &str,
        custom: Option<CustomToolInfo>,
    ) -> (usize, bool) {
        if let Some(index) = self.tool_indices.get(call_id).copied() {
            if !name.is_empty() && self.tools[index].name.is_empty() {
                self.tools[index].name = name.to_owned();
            }
            if let Some(custom) = custom {
                self.tools[index].custom = true;
                self.tools[index].response_name = custom.name;
                self.tools[index].namespace = custom.namespace;
            }
            return (index, false);
        }
        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let index = self.tools.len();
        let custom_flag = custom.is_some();
        let response_name =
            custom.as_ref().map(|tool| tool.name.clone()).unwrap_or_else(|| name.to_owned());
        let namespace = custom.and_then(|tool| tool.namespace);
        self.tools.push(LiveTool {
            call_id: call_id.to_owned(),
            item_id: format!("{}_{}", if custom_flag { "ctc" } else { "fc" }, uuid::Uuid::now_v7()),
            name: name.to_owned(),
            arguments: String::new(),
            output_index,
            ended: false,
            done_emitted: false,
            custom: custom_flag,
            response_name,
            namespace,
        });
        self.tool_indices.insert(call_id.to_owned(), index);
        self.item_order.push(LiveItem::Tool(index));
        (index, true)
    }
}
