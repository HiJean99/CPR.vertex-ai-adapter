//! OpenAI Responses 文本请求与 Vertex Gemini JSON / SSE 的有限协议转换。

use std::time::{SystemTime, UNIX_EPOCH};

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::model::{
        CanonicalEvent, ContentKind, ExecutionEvent, FinishReason, Usage, WireEvent, WirePayload,
    },
    call::upstream_adapter::UpstreamAdapterEvent,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn invalid(message: &'static str) -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, message)
}

fn text_content(value: &Value) -> Result<String, PluginFault> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(items) => {
            let mut result = String::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                if !matches!(kind, "input_text" | "output_text" | "text") {
                    return Err(invalid(
                        "Only text parts are supported by this Vertex AI adapter.",
                    ));
                }
                result.push_str(
                    item.get("text")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("Text content is missing."))?,
                );
            }
            Ok(result)
        }
        _ => Err(invalid(
            "Only text messages are supported by this Vertex AI adapter.",
        )),
    }
}

/// Returns the Vertex request body and whether the client requested SSE.
pub fn request_to_vertex(input: &[u8]) -> Result<(Vec<u8>, bool), PluginFault> {
    let root: Value = serde_json::from_slice(input)
        .map_err(|_| invalid("OpenAI Responses request must be JSON."))?;
    let object = root
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses request must be an object."))?;
    let streaming = match object.get("stream") {
        None | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        _ => return Err(invalid("Responses stream must be a boolean.")),
    };
    if object
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Err(invalid(
            "Native response continuation is not supported by Vertex AI Gemini.",
        ));
    }
    if object.get("tools").is_some_and(|value| {
        !value.is_null() && value.as_array().is_none_or(|items| !items.is_empty())
    }) {
        return Err(invalid(
            "Tools are not supported by this Vertex AI adapter.",
        ));
    }
    for unsupported in [
        "tool_choice",
        "parallel_tool_calls",
        "audio",
        "modalities",
        "include",
        "truncation",
        "reasoning",
    ] {
        if object
            .get(unsupported)
            .is_some_and(|value| !value.is_null())
        {
            return Err(invalid(
                "This Responses option is not supported by the text-only Vertex adapter.",
            ));
        }
    }

    let mut system = Vec::new();
    if let Some(instructions) = object.get("instructions") {
        system.push(
            instructions
                .as_str()
                .ok_or_else(|| invalid("Instructions must be plain text."))?
                .to_owned(),
        );
    }
    let mut contents = Vec::new();
    match object.get("input") {
        Some(Value::String(text)) => contents.push(json!({"role":"user", "parts":[{"text":text}]})),
        Some(Value::Array(messages)) => {
            for message in messages {
                let msg = message
                    .as_object()
                    .ok_or_else(|| invalid("Messages must be objects."))?;
                if msg
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind != "message")
                {
                    return Err(invalid("Only message input items are supported."));
                }
                let role = msg
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Message role is missing."))?;
                let body = msg
                    .get("content")
                    .ok_or_else(|| invalid("Message content is missing."))?;
                let text = text_content(body)?;
                match role {
                    "system" | "developer" => system.push(text),
                    "user" | "assistant" => contents.push(json!({"role": if role == "assistant" { "model" } else { "user" }, "parts":[{"text":text}]})),
                    _ => return Err(invalid("Message role is unsupported.")),
                }
            }
        }
        _ => {
            return Err(invalid(
                "Responses input must be text or an array of messages.",
            ));
        }
    }
    if contents.is_empty() {
        return Err(invalid(
            "At least one user or assistant message is required.",
        ));
    }
    let mut request = json!({"contents":contents});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"parts":[{"text":system.join("\n\n")}]});
    }
    let mut options = serde_json::Map::new();
    for (openai, vertex) in [
        ("max_output_tokens", "maxOutputTokens"),
        ("temperature", "temperature"),
        ("top_p", "topP"),
    ] {
        if let Some(value) = object.get(openai) {
            if !value.is_number()
                || value
                    .as_f64()
                    .is_none_or(|number| !number.is_finite() || number < 0.0)
            {
                return Err(invalid("Generation settings must be nonnegative numbers."));
            }
            options.insert(vertex.to_owned(), value.clone());
        }
    }
    if !options.is_empty() {
        request["generationConfig"] = Value::Object(options);
    }
    let body =
        serde_json::to_vec(&request).map_err(|_| invalid("Could not encode Vertex AI request."))?;
    Ok((body, streaming))
}

pub fn response_to_event(
    raw: &[u8],
    public_model: &str,
) -> Result<UpstreamAdapterEvent, PluginFault> {
    let root: Value =
        serde_json::from_slice(raw).map_err(|_| invalid("Vertex AI returned invalid JSON."))?;
    let candidate = root
        .pointer("/candidates/0")
        .ok_or_else(|| invalid("Vertex AI returned no candidate."))?;
    let parts = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("Vertex AI response content is unavailable."))?;
    let text = parts
        .iter()
        .filter(|part| part.get("thought") != Some(&Value::Bool(true)))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        return Err(invalid("Vertex AI returned no text output."));
    }
    let ids = StreamIds::new(public_model);
    let usage = usage_from_vertex(&root);
    let reason = finish_reason(candidate.get("finishReason").and_then(Value::as_str));
    let response = completed_response(&ids, &text, &usage);
    let wire = WireEvent {
        protocol: "openai".to_owned(),
        payload: WirePayload::RawJson {
            body: serde_json::to_vec(&response)
                .map_err(|_| invalid("Could not encode Responses output."))?,
        },
    };
    Ok(UpstreamAdapterEvent::new(ExecutionEvent {
        facts: vec![
            CanonicalEvent::Started {
                id: ids.response_id.clone(),
                model: Some(public_model.to_owned()),
            },
            CanonicalEvent::ContentAdded {
                index: 0,
                kind: ContentKind::Text,
            },
            CanonicalEvent::TextDelta { index: 0, text },
            CanonicalEvent::Usage { usage },
            CanonicalEvent::Completed {
                id: ids.response_id,
                model: Some(public_model.to_owned()),
                reason,
            },
        ],
        wire: Some(wire),
        host: None,
    }))
}

#[derive(Debug, Clone)]
pub struct StreamIds {
    pub response_id: String,
    pub item_id: String,
    pub model: String,
    pub created_at: u64,
}

impl StreamIds {
    pub fn new(model: &str) -> Self {
        let id = Uuid::new_v4().simple().to_string();
        Self {
            response_id: format!("resp_vertex_{id}"),
            item_id: format!("msg_vertex_{id}"),
            model: model.to_owned(),
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StreamDelta {
    pub text: String,
    pub usage: Option<Usage>,
    pub finish_reason: Option<FinishReason>,
}

pub fn stream_delta(raw_json: &[u8]) -> Result<StreamDelta, PluginFault> {
    let root: Value = serde_json::from_slice(raw_json)
        .map_err(|_| invalid("Vertex AI SSE data was invalid JSON."))?;
    let candidate = root.pointer("/candidates/0");
    let text = candidate
        .and_then(|c| c.pointer("/content/parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("thought") != Some(&Value::Bool(true)))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let usage = root.get("usageMetadata").map(|_| usage_from_vertex(&root));
    let finish_reason = candidate
        .and_then(|c| c.get("finishReason"))
        .and_then(Value::as_str)
        .map(|reason| finish_reason(Some(reason)));
    if text.is_empty() && usage.is_none() && finish_reason.is_none() {
        if root.get("promptFeedback").is_some() {
            return Err(invalid("Vertex AI blocked this prompt."));
        }
        return Err(invalid("Vertex AI SSE event contained no text or usage."));
    }
    Ok(StreamDelta {
        text,
        usage,
        finish_reason,
    })
}

pub fn stream_start_events(ids: &StreamIds) -> Vec<UpstreamAdapterEvent> {
    let response = json!({"id":ids.response_id,"object":"response","created_at":ids.created_at,"status":"in_progress","model":ids.model,"output":[]});
    let item = json!({"id":ids.item_id,"type":"message","role":"assistant","status":"in_progress","content":[]});
    let part = json!({"type":"output_text","text":"","annotations":[]});
    vec![
        stream_wire_event(
            "response.created",
            json!({"type":"response.created","sequence_number":0,"response":response}),
            Some(CanonicalEvent::Started {
                id: ids.response_id.clone(),
                model: Some(ids.model.clone()),
            }),
        ),
        stream_wire_event(
            "response.output_item.added",
            json!({"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":item}),
            None,
        ),
        stream_wire_event(
            "response.content_part.added",
            json!({"type":"response.content_part.added","sequence_number":2,"item_id":ids.item_id,"output_index":0,"content_index":0,"part":part}),
            Some(CanonicalEvent::ContentAdded {
                index: 0,
                kind: ContentKind::Text,
            }),
        ),
    ]
}

pub fn stream_text_event(ids: &StreamIds, sequence: u64, text: &str) -> UpstreamAdapterEvent {
    stream_wire_event(
        "response.output_text.delta",
        json!({"type":"response.output_text.delta","sequence_number":sequence,"item_id":ids.item_id,"output_index":0,"content_index":0,"delta":text}),
        Some(CanonicalEvent::TextDelta {
            index: 0,
            text: text.to_owned(),
        }),
    )
}

pub fn stream_complete_events(
    ids: &StreamIds,
    sequence: u64,
    full_text: &str,
    usage: Usage,
    reason: FinishReason,
) -> Vec<UpstreamAdapterEvent> {
    let response = completed_response(ids, full_text, &usage);
    vec![
        stream_wire_event(
            "response.output_text.done",
            json!({"type":"response.output_text.done","sequence_number":sequence,"item_id":ids.item_id,"output_index":0,"content_index":0,"text":full_text}),
            None,
        ),
        stream_wire_event(
            "response.content_part.done",
            json!({"type":"response.content_part.done","sequence_number":sequence+1,"item_id":ids.item_id,"output_index":0,"content_index":0,"part":{"type":"output_text","text":full_text,"annotations":[]}}),
            None,
        ),
        stream_wire_event(
            "response.output_item.done",
            json!({"type":"response.output_item.done","sequence_number":sequence+2,"output_index":0,"item":{"id":ids.item_id,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":full_text,"annotations":[]}]}}),
            None,
        ),
        stream_wire_event(
            "response.completed",
            json!({"type":"response.completed","sequence_number":sequence+3,"response":response}),
            Some(CanonicalEvent::Usage { usage }),
        ),
        UpstreamAdapterEvent::new(ExecutionEvent::canonical(CanonicalEvent::Completed {
            id: ids.response_id.clone(),
            model: Some(ids.model.clone()),
            reason,
        })),
    ]
}

fn stream_wire_event(
    event_name: &str,
    data: Value,
    fact: Option<CanonicalEvent>,
) -> UpstreamAdapterEvent {
    let serialized = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_owned());
    let frame = format!("event: {event_name}\ndata: {serialized}\n\n").into_bytes();
    let mut event = ExecutionEvent::wire(WireEvent {
        protocol: "openai".to_owned(),
        payload: WirePayload::RawSse { frame },
    });
    if let Some(fact) = fact {
        event.facts.push(fact);
    }
    UpstreamAdapterEvent::new(event)
}

fn completed_response(ids: &StreamIds, text: &str, usage: &Usage) -> Value {
    json!({
        "id":ids.response_id,"object":"response","created_at":ids.created_at,"status":"completed","model":ids.model,
        "output":[{"id":ids.item_id,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}],
        "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"total_tokens":usage.total_tokens}
    })
}

pub fn usage_from_vertex(root: &Value) -> Usage {
    let prompt = root
        .pointer("/usageMetadata/promptTokenCount")
        .and_then(Value::as_u64);
    let output = root
        .pointer("/usageMetadata/candidatesTokenCount")
        .and_then(Value::as_u64);
    let cached = root
        .pointer("/usageMetadata/cachedContentTokenCount")
        .and_then(Value::as_u64);
    let total = root
        .pointer("/usageMetadata/totalTokenCount")
        .and_then(Value::as_u64)
        .or_else(|| prompt.zip(output).map(|(a, b)| a.saturating_add(b)));
    Usage {
        input_tokens: prompt,
        output_tokens: output,
        cached_tokens: cached,
        total_tokens: total,
        ..Usage::default()
    }
}

pub fn finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            FinishReason::ContentFilter
        }
        Some("STOP") | None => FinishReason::Stop,
        _ => FinishReason::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_openai_responses_and_preserves_stream_flag() {
        let input = json!({"model":"vertex/google/gemini-2.5-flash","instructions":"Be concise","input":[{"role":"user","content":[{"type":"input_text","text":"Hello"}]},{"role":"assistant","content":[{"type":"output_text","text":"Hi"}]}],"temperature":0.2});
        let (body, stream) = request_to_vertex(input.to_string().as_bytes()).unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert!(!stream);
        assert_eq!(result.pointer("/contents/0/parts/0/text").unwrap(), "Hello");
        assert_eq!(result.pointer("/contents/1/role").unwrap(), "model");
        assert_eq!(
            result.pointer("/systemInstruction/parts/0/text").unwrap(),
            "Be concise"
        );
        assert_eq!(
            result.pointer("/generationConfig/temperature").unwrap(),
            0.2
        );
        let (stream_body, stream) =
            request_to_vertex(br#"{"input":"Hello","stream":true}"#).unwrap();
        assert!(stream);
        assert!(serde_json::from_slice::<Value>(&stream_body).is_ok());
    }

    #[test]
    fn rejects_tools_continuation_and_non_text() {
        for input in [
            json!({"input":"Hi","tools":[{"type":"function"}]}),
            json!({"input":"Hi","previous_response_id":"resp_other"}),
            json!({"input":[{"role":"user","content":[{"type":"input_image"}]}]}),
        ] {
            assert!(request_to_vertex(input.to_string().as_bytes()).is_err());
        }
    }

    #[test]
    fn converts_non_stream_response_and_usage() {
        let raw = json!({"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12}});
        let event = response_to_event(raw.to_string().as_bytes(), "vertex/google/gemini-2.5-flash")
            .unwrap();
        let decoded = UpstreamAdapterEvent::decode(&event.encode().unwrap()).unwrap();
        assert_eq!(decoded.event.facts.len(), 5);
        assert!(
            matches!(&decoded.event.facts[3], CanonicalEvent::Usage { usage } if usage.input_tokens == Some(10) && usage.output_tokens == Some(2))
        );
    }

    #[test]
    fn builds_openai_responses_sse_events() {
        let ids = StreamIds::new("vertex/google/gemini-2.5-flash");
        let start = stream_start_events(&ids);
        assert_eq!(start.len(), 3);
        let delta = stream_text_event(&ids, 3, "Hello");
        let decoded = UpstreamAdapterEvent::decode(&delta.encode().unwrap()).unwrap();
        assert!(
            matches!(&decoded.event.facts[0], CanonicalEvent::TextDelta { text, .. } if text == "Hello")
        );
        assert!(
            matches!(&decoded.event.wire.unwrap().payload, WirePayload::RawSse { frame } if std::str::from_utf8(frame).unwrap().contains("response.output_text.delta"))
        );
    }
}
