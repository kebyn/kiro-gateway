use bytes::{Buf, Bytes, BytesMut};
use crc::{CRC_32_ISO_HDLC, Crc};
use serde_json::Value;

use super::error::UpstreamStreamError;
use crate::protocol::internal::InternalEvent;

const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);

#[derive(Clone, Debug, PartialEq)]
pub struct EventMessage {
    pub headers: Vec<(String, String)>,
    pub payload: Bytes,
}

pub struct EventStreamDecoder {
    buffer: BytesMut,
    max_frame_size: usize,
}

impl Default for EventStreamDecoder {
    fn default() -> Self {
        Self { buffer: BytesMut::new(), max_frame_size: 16 * 1024 * 1024 }
    }
}
impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<EventMessage>, UpstreamStreamError> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(message) = self.decode_one()? {
            out.push(message);
        }
        Ok(out)
    }
    pub fn finish(&self) -> Result<(), UpstreamStreamError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(UpstreamStreamError::TruncatedFrame { expected: 12, received: self.buffer.len() })
        }
    }
    fn decode_one(&mut self) -> Result<Option<EventMessage>, UpstreamStreamError> {
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let total_len = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
        let headers_len = u32::from_be_bytes(self.buffer[4..8].try_into().unwrap()) as usize;
        if !(16..=self.max_frame_size).contains(&total_len) || headers_len > total_len - 16 {
            return Err(UpstreamStreamError::InvalidLength(total_len as u32));
        }
        if self.buffer.len() < total_len {
            return Ok(None);
        }
        let frame = self.buffer.split_to(total_len).freeze();
        let expected_crc = u32::from_be_bytes(frame[total_len - 4..].try_into().unwrap());
        if CRC32.checksum(&frame[..total_len - 4]) != expected_crc {
            return Err(UpstreamStreamError::CrcMismatch);
        }
        let prelude_crc = u32::from_be_bytes(frame[8..12].try_into().unwrap());
        if CRC32.checksum(&frame[..8]) != prelude_crc {
            return Err(UpstreamStreamError::CrcMismatch);
        }
        let headers = parse_headers(&frame[12..12 + headers_len])?;
        let payload_start = 12 + headers_len;
        Ok(Some(EventMessage { headers, payload: frame.slice(payload_start..total_len - 4) }))
    }
}

fn parse_headers(mut bytes: &[u8]) -> Result<Vec<(String, String)>, UpstreamStreamError> {
    let mut headers = Vec::new();
    while !bytes.is_empty() {
        let name_len = bytes[0] as usize;
        bytes.advance(1);
        if name_len == 0 || bytes.len() < name_len + 1 {
            return Err(UpstreamStreamError::MalformedHeader);
        }
        let name = String::from_utf8(bytes[..name_len].to_vec())
            .map_err(|_| UpstreamStreamError::MalformedHeader)?;
        bytes.advance(name_len);
        let value_type = bytes[0];
        bytes.advance(1);
        let fixed_len = match value_type {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                if bytes.len() < 2 {
                    return Err(UpstreamStreamError::MalformedHeader);
                }
                let len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                bytes.advance(2);
                len
            }
            _ => return Err(UpstreamStreamError::MalformedHeader),
        };
        if bytes.len() < fixed_len {
            return Err(UpstreamStreamError::MalformedHeader);
        }
        if value_type == 7 {
            let value = String::from_utf8(bytes[..fixed_len].to_vec())
                .map_err(|_| UpstreamStreamError::MalformedHeader)?;
            headers.push((name, value));
        }
        bytes.advance(fixed_len);
    }
    Ok(headers)
}

pub fn decode_internal_events(
    message: &EventMessage,
) -> Result<Vec<InternalEvent>, UpstreamStreamError> {
    let parsed = serde_json::from_slice::<Value>(&message.payload);
    let message_type = header(message, ":message-type").unwrap_or("event");
    if matches!(message_type, "error" | "exception") {
        let detail = parsed
            .as_ref()
            .ok()
            .and_then(|value| value.get("message"))
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or(message_type);
        return Err(UpstreamStreamError::Upstream(detail.to_owned()));
    }
    let value = parsed.map_err(|e| UpstreamStreamError::Event(e.to_string()))?;
    let kind = message
        .headers
        .iter()
        .find(|(k, _)| k == ":event-type")
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| value.get("eventType").and_then(Value::as_str).unwrap_or(""));
    match kind {
        "assistantResponseEvent" | "assistant_response" => Ok(vec![InternalEvent::TextDelta {
            text: value
                .get("content")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }]),
        "reasoningContentEvent" | "reasoning_content" => Ok(vec![InternalEvent::ThinkingDelta {
            text: value
                .get("text")
                .or_else(|| value.get("content"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }]),
        "toolUseEvent" | "tool_use" => {
            let id = string_field(&value, &["toolUseId", "toolUseID", "tool_use_id", "id"])
                .unwrap_or_default()
                .to_owned();
            let name = string_field(&value, &["name", "toolName", "tool_name"]);
            let mut events = Vec::new();
            if name.is_some() || !id.is_empty() {
                events.push(InternalEvent::ToolCallStart {
                    id: id.clone(),
                    name: name.unwrap_or_default().to_owned(),
                });
            }
            if let Some(arguments) = value.get("input").or_else(|| value.get("content")) {
                events.push(InternalEvent::ToolCallDelta {
                    id: id.clone(),
                    arguments: arguments.clone(),
                });
            }
            if bool_field(&value, &["stop", "isStop", "done"]) {
                events.push(InternalEvent::ToolCallEnd { id, complete: true });
            }
            Ok(events)
        }
        "metadataEvent" | "metadata" => {
            let input_tokens = number_field(&value, &["inputTokens", "input_tokens"]);
            let output_tokens = number_field(&value, &["outputTokens", "output_tokens"]);
            let mut events = Vec::new();
            if input_tokens.is_some() || output_tokens.is_some() {
                events.push(InternalEvent::Usage {
                    usage: crate::protocol::internal::Usage::new(
                        input_tokens.unwrap_or_default(),
                        output_tokens.unwrap_or_default(),
                    ),
                });
            }
            if let Some(reason) = string_field(&value, &["stopReason", "stop_reason"]) {
                events.push(InternalEvent::Stop { reason: reason.to_owned() });
            }
            Ok(events)
        }
        "contextUsageEvent" | "context_usage" | "meteringEvent" | "metering" => Ok(Vec::new()),
        "error" | "errorEvent" => Ok(vec![InternalEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error")
                .to_owned(),
        }]),
        // EventStream is extensible; unrecognized event types carry optional
        // metadata and must not invalidate an otherwise usable response.
        _ => Ok(Vec::new()),
    }
}

fn header<'a>(message: &'a EventMessage, name: &str) -> Option<&'a str> {
    message.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

fn string_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

fn bool_field(value: &Value, names: &[&str]) -> bool {
    names.iter().find_map(|name| value.get(*name).and_then(Value::as_bool)).unwrap_or(false)
}

fn number_field(value: &Value, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .or_else(|| value.get("usage").and_then(|usage| usage.get(*name)))
            .and_then(Value::as_u64)
    })
}

#[cfg(test)]
mod tests {
    use super::{CRC32, EventStreamDecoder, decode_internal_events};
    use crate::protocol::internal::InternalEvent;

    fn frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        frame_with_headers(&[(":event-type", event_type)], payload)
    }

    fn frame_with_headers(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
        let mut encoded_headers = Vec::new();
        for (name, value) in headers {
            encoded_headers.push(name.len() as u8);
            encoded_headers.extend_from_slice(name.as_bytes());
            encoded_headers.push(7);
            encoded_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            encoded_headers.extend_from_slice(value.as_bytes());
        }
        let total = 16 + encoded_headers.len() + payload.len();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(encoded_headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&CRC32.checksum(&frame).to_be_bytes());
        frame.extend_from_slice(&encoded_headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&CRC32.checksum(&frame).to_be_bytes());
        frame
    }

    #[test]
    fn decodes_partial_frame_and_payload() {
        let input = frame("assistantResponseEvent", br#"{"content":"hi"}"#);
        let mut decoder = EventStreamDecoder::new();
        assert!(decoder.push(&input[..5]).unwrap().is_empty());
        let messages = decoder.push(&input[5..]).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload, br#"{"content":"hi"}"#.as_slice());
        decoder.finish().unwrap();
    }

    #[test]
    fn rejects_bad_crc() {
        let mut input = frame("metadataEvent", b"{}");
        let last = input.len() - 1;
        input[last] ^= 1;
        let mut decoder = EventStreamDecoder::new();
        assert!(matches!(decoder.push(&input), Err(super::UpstreamStreamError::CrcMismatch)));
    }

    #[test]
    fn decodes_reasoning_and_metadata_stop_reason() {
        let input = [
            frame("reasoningContentEvent", br#"{"text":"thinking"}"#),
            frame(
                "metadataEvent",
                br#"{"usage":{"inputTokens":3,"outputTokens":5},"stop_reason":"MAX_TOKENS"}"#,
            ),
        ]
        .concat();
        let mut decoder = EventStreamDecoder::new();
        let messages = decoder.push(&input).unwrap();
        let events: Vec<_> =
            messages.iter().flat_map(|message| decode_internal_events(message).unwrap()).collect();
        assert_eq!(
            events,
            [
                InternalEvent::ThinkingDelta { text: "thinking".into() },
                InternalEvent::Usage { usage: crate::protocol::internal::Usage::new(3, 5) },
                InternalEvent::Stop { reason: "MAX_TOKENS".into() },
            ]
        );
    }

    #[test]
    fn rejects_upstream_exception_message() {
        let input = frame_with_headers(
            &[(":message-type", "exception"), (":event-type", "error")],
            br#"{"message":"upstream denied request"}"#,
        );
        let mut decoder = EventStreamDecoder::new();
        let message = decoder.push(&input).unwrap().pop().unwrap();
        let error = decode_internal_events(&message).unwrap_err();
        assert_eq!(error.to_string(), "upstream exception: upstream denied request");
    }
}
