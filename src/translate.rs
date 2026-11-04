//! OpenAI Responses 文本请求与 Vertex Gemini 4 Argon JSON / SSE 的协议转换。

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

const MAX_NON_STREAMING_OUTPUT_TOKENS: u64 = 32_768;
const MAX_STREAMING_OUTPUT_TOKENS: u64 = 65_536;

fn invalid(message: &'static str) -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, message)
}

fn thinking_level(
    object: &serde_json::Map<String, Value>,
) -> Result<Option<&'static str>, PluginFault> {
    let Some(reasoning) = object.get("reasoning").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let reasoning = reasoning
        .as_object()
        .ok_or_else(|| invalid("Reasoning must be an object with an effort field."))?;
    if reasoning
        .keys()
        .any(|key| key != "effort" && key != "summary")
    {
        return Err(invalid(
            "Only reasoning.effort and reasoning.summary are accepted by the Gemini 4 adapter.",
        ));
    }
    if let Some(summary) = reasoning.get("summary").filter(|value| !value.is_null())
        && !matches!(summary.as_str(), Some("auto" | "concise" | "detailed"))
    {
        return Err(invalid(
            "Reasoning summary must be auto, concise, or detailed.",
        ));
    }
    let Some(effort) = reasoning.get("effort").filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let effort = effort
        .as_str()
        .ok_or_else(|| invalid("Reasoning effort must be low, medium, or high."))?;
    match effort.to_ascii_lowercase().as_str() {
        "low" => Ok(Some("LOW")),
        "medium" => Ok(Some("MEDIUM")),
        "high" => Ok(Some("HIGH")),
        _ => Err(invalid(
            "Gemini 4 supports reasoning effort low, medium, or high.",
        )),
    }
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

/// Returns the Vertex request body and whether the downstream client requested SSE output.
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
        "frequency_penalty",
        "presence_penalty",
        "candidate_count",
        "thinking_budget",
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
    if contents
        .last()
        .and_then(|content| content.get("role"))
        .and_then(Value::as_str)
        == Some("model")
    {
        return Err(invalid(
            "Gemini 4 input history cannot end with a prefilled model turn.",
        ));
    }
    let mut request = json!({"contents":contents});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"parts":[{"text":system.join("\n\n")}]});
    }
    let mut options = serde_json::Map::new();
    if let Some(value) = object
        .get("max_output_tokens")
        .filter(|value| !value.is_null())
    {
        let maximum = if streaming {
            MAX_STREAMING_OUTPUT_TOKENS
        } else {
            MAX_NON_STREAMING_OUTPUT_TOKENS
        };
        let tokens = value
            .as_u64()
            .filter(|tokens| (1..=maximum).contains(tokens))
            .ok_or_else(|| {
                if streaming {
                    invalid("Gemini 4 streaming requests support max_output_tokens from 1 to 65536.")
                } else {
                    invalid("Gemini 4 non-streaming requests support max_output_tokens from 1 to 32768.")
                }
            })?;
        options.insert("maxOutputTokens".to_owned(), Value::from(tokens));
    }
    if let Some(level) = thinking_level(object)? {
        options.insert(
            "thinkingConfig".to_owned(),
            json!({"thinkingLevel":level,"includeThoughts":true}),
        );
    }
    // Gemini 4 ignores temperature, top_p and top_k; do not forward misleading controls.
    if !options.is_empty() {
        request["generationConfig"] = Value::Object(options);
    }
    let body =
        serde_json::to_vec(&request).map_err(|_| invalid("Could not encode Vertex AI request."))?;
    Ok((body, streaming))
}

/// Build the single terminal JSON event after collecting Vertex SSE for a downstream stream:false call.
pub fn completed_response_event(
    ids: &StreamIds,
    text: &str,
    usage: Usage,
    reason: FinishReason,
) -> UpstreamAdapterEvent {
    let response = completed_response(ids, text, &usage);
    let wire = WireEvent {
        protocol: "openai".to_owned(),
        payload: WirePayload::Json {
            event: Some("response.completed".to_owned()),
            data: json!({
                "type": "response.completed",
                "sequence_number": 0,
                "response": response,
            }),
            id: None,
            retry: None,
            raw_sse: None,
        },
    };
    UpstreamAdapterEvent::new(ExecutionEvent {
        facts: vec![
            CanonicalEvent::Started {
                id: ids.response_id.clone(),
                model: Some(ids.model.clone()),
            },
            CanonicalEvent::ContentAdded {
                index: 0,
                kind: ContentKind::Text,
            },
            CanonicalEvent::TextDelta {
                index: 0,
                text: text.to_owned(),
            },
            CanonicalEvent::Usage { usage },
            CanonicalEvent::Completed {
                id: ids.response_id.clone(),
                model: Some(ids.model.clone()),
                reason,
            },
        ],
        wire: Some(wire),
        host: None,
    })
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
    let parts = candidate
        .and_then(|c| c.pointer("/content/parts"))
        .and_then(Value::as_array);
    let has_thought = parts.is_some_and(|parts| {
        parts
            .iter()
            .any(|part| part.get("thought") == Some(&Value::Bool(true)))
    });
    let text = parts
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
        if has_thought {
            // Thought-only frames are upstream keepalives; their content is never sent downstream.
            return Ok(StreamDelta {
                text,
                usage,
                finish_reason,
            });
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
    let mut response = json!({
        "id":ids.response_id,"object":"response","created_at":ids.created_at,"status":"completed","model":ids.model,
        "output":[{"id":ids.item_id,"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}],
        "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"total_tokens":usage.total_tokens}
    });
    if let Some(reasoning_tokens) = usage.reasoning_tokens {
        response["usage"]["output_tokens_details"] = json!({"reasoning_tokens":reasoning_tokens});
    }
    response
}

pub fn usage_from_vertex(root: &Value) -> Usage {
    let prompt = root
        .pointer("/usageMetadata/promptTokenCount")
        .and_then(Value::as_u64);
    let candidate_output = root
        .pointer("/usageMetadata/candidatesTokenCount")
        .and_then(Value::as_u64);
    let cached = root
        .pointer("/usageMetadata/cachedContentTokenCount")
        .and_then(Value::as_u64);
    let reasoning = root
        .pointer("/usageMetadata/thoughtsTokenCount")
        .and_then(Value::as_u64);
    let output = candidate_output
        .zip(reasoning)
        .map(|(candidate, thoughts)| candidate.saturating_add(thoughts))
        .or(candidate_output)
        .or(reasoning);
    let total = root
        .pointer("/usageMetadata/totalTokenCount")
        .and_then(Value::as_u64)
        .or_else(|| prompt.zip(output).map(|(a, b)| a.saturating_add(b)));
    Usage {
        input_tokens: prompt,
        output_tokens: output,
        cached_tokens: cached,
        reasoning_tokens: reasoning,
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
    fn translates_responses_and_gemini_4_reasoning_effort() {
        let input = json!({
            "model":"vertex/google/gemini-4-argon",
            "instructions":"Be concise",
            "input":[
                {"role":"user","content":[{"type":"input_text","text":"Hello"}]},
                {"role":"assistant","content":[{"type":"output_text","text":"Hi"}]},
                {"role":"user","content":[{"type":"input_text","text":"Continue"}]}
            ],
            "max_output_tokens":1024,
            "reasoning":{"effort":"HIGH"},
            "temperature":0.2,
            "top_p":0.8,
            "top_k":20
        });
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
            result.pointer("/generationConfig/maxOutputTokens").unwrap(),
            1024
        );
        assert_eq!(
            result
                .pointer("/generationConfig/thinkingConfig/thinkingLevel")
                .unwrap(),
            "HIGH"
        );
        assert_eq!(
            result
                .pointer("/generationConfig/thinkingConfig/includeThoughts")
                .unwrap(),
            true
        );
        assert!(result.pointer("/generationConfig/temperature").is_none());
        assert!(result.pointer("/generationConfig/topP").is_none());
        assert!(result.pointer("/generationConfig/topK").is_none());

        let (stream_body, stream) =
            request_to_vertex(br#"{"input":"Hello","stream":true}"#).unwrap();
        assert!(stream);
        assert!(serde_json::from_slice::<Value>(&stream_body).is_ok());
    }

    #[test]
    fn defaults_to_gemini_4_medium_thinking_and_rejects_unsupported_levels() {
        let (body, _) = request_to_vertex(br#"{"input":"Hello"}"#).unwrap();
        let result: Value = serde_json::from_slice(&body).unwrap();
        assert!(result.pointer("/generationConfig/thinkingConfig").is_none());

        for effort in ["minimal", "none", "xhigh"] {
            let input = json!({"input":"Hello","reasoning":{"effort":effort}});
            assert!(request_to_vertex(input.to_string().as_bytes()).is_err());
        }
        for (effort, expected) in [("low", "LOW"), ("medium", "MEDIUM"), ("high", "HIGH")] {
            let input = json!({
                "input": "Hello",
                "reasoning": {"effort": effort, "summary": "auto"}
            });
            let (body, _) = request_to_vertex(input.to_string().as_bytes()).unwrap();
            let result: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                result
                    .pointer("/generationConfig/thinkingConfig/thinkingLevel")
                    .unwrap(),
                expected
            );
            assert_eq!(
                result
                    .pointer("/generationConfig/thinkingConfig/includeThoughts")
                    .unwrap(),
                true
            );
        }
        assert!(
            request_to_vertex(
                br#"{"input":"Hello","reasoning":{"effort":"high","summary":"verbose"}}"#
            )
            .is_err()
        );
        for field in [
            "frequency_penalty",
            "presence_penalty",
            "candidate_count",
            "thinking_budget",
        ] {
            let input = format!(r#"{{"input":"Hello","{field}":1}}"#);
            assert!(request_to_vertex(input.as_bytes()).is_err());
        }
        assert!(
            request_to_vertex(br#"{"input":[{"role":"assistant","content":"prefilled"}]}"#)
                .is_err()
        );
    }

    #[test]
    fn enforces_gemini_4_single_request_output_limits() {
        assert!(request_to_vertex(br#"{"input":"Hello","max_output_tokens":32769}"#).is_err());
        assert!(
            request_to_vertex(br#"{"input":"Hello","stream":true,"max_output_tokens":65536}"#)
                .is_ok()
        );
        assert!(
            request_to_vertex(br#"{"input":"Hello","stream":true,"max_output_tokens":65537}"#)
                .is_err()
        );
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
    fn converts_collected_sse_to_completed_wire_and_preserves_usage() {
        let raw = json!({"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":7,"totalTokenCount":19}});
        let usage = usage_from_vertex(&raw);
        let ids = StreamIds::new("vertex/google/gemini-4-argon");
        let event = completed_response_event(&ids, "Hello", usage, FinishReason::Stop);
        let decoded = UpstreamAdapterEvent::decode(&event.encode().unwrap()).unwrap();

        assert_eq!(decoded.event.facts.len(), 5);
        assert!(
            matches!(&decoded.event.facts[3], CanonicalEvent::Usage { usage } if usage.input_tokens == Some(10) && usage.output_tokens == Some(9) && usage.reasoning_tokens == Some(7) && usage.total_tokens == Some(19))
        );
        assert!(matches!(
            &decoded.event.facts[4],
            CanonicalEvent::Completed {
                reason: FinishReason::Stop,
                ..
            }
        ));

        let wire = decoded.event.wire.expect("non-stream response wire");
        assert_eq!(wire.protocol, "openai");
        let WirePayload::Json {
            event: Some(event_type),
            data,
            id: None,
            retry: None,
            raw_sse: None,
        } = wire.payload
        else {
            panic!("non-stream response must use a typed JSON wire event");
        };
        assert_eq!(event_type, "response.completed");
        assert_eq!(data["type"], "response.completed");
        assert_eq!(data["sequence_number"], 0);
        assert_eq!(data["response"]["status"], "completed");
        assert_eq!(data["response"]["model"], "vertex/google/gemini-4-argon");
        assert_eq!(data["response"]["output"][0]["content"][0]["text"], "Hello");
        assert_eq!(data["response"]["usage"]["input_tokens"], 10);
        assert_eq!(data["response"]["usage"]["output_tokens"], 9);
        assert_eq!(
            data["response"]["usage"]["output_tokens_details"]["reasoning_tokens"],
            7
        );
        assert_eq!(data["response"]["usage"]["total_tokens"], 19);
    }

    #[test]
    fn thought_only_sse_frames_are_keepalives_and_thought_text_is_never_exposed() {
        let heartbeat = stream_delta(
            br#"{"candidates":[{"content":{"parts":[{"text":"private thought","thought":true}]}}]}"#,
        )
        .unwrap();
        assert!(heartbeat.text.is_empty());
        assert!(heartbeat.usage.is_none());
        assert!(heartbeat.finish_reason.is_none());

        let mixed = stream_delta(
            br#"{"candidates":[{"content":{"parts":[{"text":"private thought","thought":true},{"text":"visible answer"}]}}]}"#,
        )
        .unwrap();
        assert_eq!(mixed.text, "visible answer");
    }

    #[test]
    fn builds_openai_responses_sse_events() {
        let ids = StreamIds::new("vertex/google/gemini-4-argon");
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
