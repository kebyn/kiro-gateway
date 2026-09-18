use bytes::{Buf, Bytes, BytesMut};
use crc::{CRC_32_ISCSI, Crc};
use serde_json::Value;

use super::error::UpstreamStreamError;
use crate::protocol::internal::InternalEvent;

const CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

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
        if CRC32C.checksum(&frame[..total_len - 4]) != expected_crc {
            return Err(UpstreamStreamError::CrcMismatch);
        }
        let prelude_crc = u32::from_be_bytes(frame[8..12].try_into().unwrap());
        if CRC32C.checksum(&frame[..8]) != prelude_crc {
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
        if bytes.is_empty() {
            return Err(UpstreamStreamError::MalformedHeader);
        }
        let name_len = bytes[0] as usize;
        bytes.advance(1);
        if bytes.len() < name_len + 1 {
            return Err(UpstreamStreamError::MalformedHeader);
        }
        let name = String::from_utf8(bytes[..name_len].to_vec())
            .map_err(|_| UpstreamStreamError::MalformedHeader)?;
        bytes.advance(name_len);
        let value_type = bytes[0];
        bytes.advance(1);
        let value = match value_type {
            7 => {
                if bytes.len() < 2 {
                    return Err(UpstreamStreamError::MalformedHeader);
                }
                let len = u16::from_be_bytes([bytes[0], bytes[1]]) as usize;
                bytes.advance(2);
                if bytes.len() < len {
                    return Err(UpstreamStreamError::MalformedHeader);
                }
                let value = String::from_utf8(bytes[..len].to_vec())
                    .map_err(|_| UpstreamStreamError::MalformedHeader)?;
                bytes.advance(len);
                value
            }
            6 => {
                if bytes.len() < 4 {
                    return Err(UpstreamStreamError::MalformedHeader);
                }
                let value = u32::from_be_bytes(bytes[..4].try_into().unwrap()).to_string();
                bytes.advance(4);
                value
            }
            _ => return Err(UpstreamStreamError::MalformedHeader),
        };
        headers.push((name, value));
    }
    Ok(headers)
}

pub fn decode_internal_event(message: &EventMessage) -> Result<InternalEvent, UpstreamStreamError> {
    let value: Value = serde_json::from_slice(&message.payload)
        .map_err(|e| UpstreamStreamError::Event(e.to_string()))?;
    let kind = message
        .headers
        .iter()
        .find(|(k, _)| k == ":event-type")
        .map(|(_, v)| v.as_str())
        .unwrap_or_else(|| value.get("eventType").and_then(Value::as_str).unwrap_or(""));
    match kind {
        "assistantResponseEvent" | "assistant_response" => Ok(InternalEvent::TextDelta {
            text: value
                .get("content")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }),
        "toolUseEvent" | "tool_use" => Ok(InternalEvent::ToolCallDelta {
            id: value
                .get("toolUseId")
                .or_else(|| value.get("tool_use_id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            arguments: value
                .get("input")
                .or_else(|| value.get("content"))
                .map(Value::to_string)
                .unwrap_or_default(),
            name: value
                .get("name")
                .or_else(|| value.get("toolName"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        }),
        "metadataEvent" | "metadata" => Ok(InternalEvent::Usage {
            usage: crate::protocol::internal::Usage::new(
                value.get("inputTokens").and_then(Value::as_u64).unwrap_or_default(),
                value.get("outputTokens").and_then(Value::as_u64).unwrap_or_default(),
            ),
        }),
        "contextUsageEvent" | "context_usage" => Ok(InternalEvent::Usage {
            usage: crate::protocol::internal::Usage::new(
                value.get("inputTokens").and_then(Value::as_u64).unwrap_or_default(),
                0,
            ),
        }),
        "error" | "errorEvent" => Ok(InternalEvent::Error {
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("upstream error")
                .to_owned(),
        }),
        _ => Err(UpstreamStreamError::Event(format!("unknown event type {kind}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::{CRC32C, EventStreamDecoder};

    fn frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.push(11);
        headers.extend_from_slice(b":event-type");
        headers.push(7);
        headers.extend_from_slice(&(event_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(event_type.as_bytes());
        let total = 16 + headers.len() + payload.len();
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&CRC32C.checksum(&frame).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&CRC32C.checksum(&frame).to_be_bytes());
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
}
