//! 在 OpenAI Responses 文本请求与 Vertex Gemini JSON 间进行显式、有限的协议转换

use std::time::{SystemTime, UNIX_EPOCH};

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        model::{
            CanonicalEvent, ContentKind, ExecutionEvent, FinishReason, Usage, WireEvent,
            WirePayload,
        },
        upstream_adapter::UpstreamAdapterEvent,
    },
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
                let text = item
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("Text content is missing."))?;
                result.push_str(text);
            }
            Ok(result)
        }
        _ => Err(invalid(
            "Only text messages are supported by this Vertex AI adapter.",
        )),
    }
}

pub fn request_to_vertex(input: &[u8]) -> Result<Vec<u8>, PluginFault> {
    let root: Value = serde_json::from_slice(input)
        .map_err(|_| invalid("OpenAI Responses request must be JSON."))?;
    let object = root
        .as_object()
        .ok_or_else(|| invalid("OpenAI Responses request must be an object."))?;
    if object.get("stream").and_then(Value::as_bool) != Some(false) {
        return Err(invalid("Set stream=false; streaming is not supported yet."));
    }
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
        Some(Value::String(text)) => {
            contents.push(json!({"role":"user", "parts":[{"text":text}]}));
        }
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
                    "user" | "assistant" => contents.push(json!({
                        "role": if role == "assistant" { "model" } else { "user" },
                        "parts": [{"text": text}]
                    })),
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
    serde_json::to_vec(&request).map_err(|_| invalid("Could not encode Vertex AI request."))
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
    let generated_id = Uuid::new_v4().simple();
    let response_id = format!("resp_vertex_{generated_id}");
    let item_id = format!("msg_vertex_{generated_id}");
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
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
    let usage = Usage {
        input_tokens: prompt,
        output_tokens: output,
        cached_tokens: cached,
        total_tokens: total,
        ..Usage::default()
    };
    let reason = match candidate.get("finishReason").and_then(Value::as_str) {
        Some("MAX_TOKENS") => FinishReason::Length,
        Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            FinishReason::ContentFilter
        }
        _ => FinishReason::Stop,
    };
    let response = json!({
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": "completed",
        "model": public_model,
        "output": [{
            "id": item_id,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }]
        }],
        "usage": {
            "input_tokens": prompt,
            "output_tokens": output,
            "total_tokens": total
        }
    });
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
                id: response_id.clone(),
                model: Some(public_model.to_owned()),
            },
            CanonicalEvent::ContentAdded {
                index: 0,
                kind: ContentKind::Text,
            },
            CanonicalEvent::TextDelta { index: 0, text },
            CanonicalEvent::Usage { usage },
            CanonicalEvent::Completed {
                id: response_id,
                model: Some(public_model.to_owned()),
                reason,
            },
        ],
        wire: Some(wire),
        host: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_messages_and_instructions() {
        let input = json!({
            "model":"vertex/google/gemini-2.5-flash", "stream":false,
            "instructions":"Be concise", "input":[
                {"role":"user", "content":[{"type":"input_text","text":"Hello"}]},
                {"role":"assistant", "content":[{"type":"output_text","text":"Hi"}]}
            ], "temperature":0.2
        });
        let result: Value =
            serde_json::from_slice(&request_to_vertex(input.to_string().as_bytes()).unwrap())
                .unwrap();
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
    }

    #[test]
    fn rejects_stream_and_tool_inputs() {
        for input in [
            json!({"input":"Hi","stream":true}),
            json!({"input":"Hi","stream":false,"tools":[{"type":"function"}]}),
            json!({"input":"Hi","stream":false,"previous_response_id":"resp_other"}),
            json!({"input":[{"role":"user","content":[{"type":"input_image"}]}],"stream":false}),
        ] {
            assert!(request_to_vertex(input.to_string().as_bytes()).is_err());
        }
    }

    #[test]
    fn converts_completion_and_usage() {
        let raw = json!({
            "candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]} ,"finishReason":"STOP"}],
            "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"totalTokenCount":12}
        });
        let event = response_to_event(raw.to_string().as_bytes(), "vertex/google/gemini-2.5-flash")
            .unwrap();
        let decoded = UpstreamAdapterEvent::decode(&event.encode().unwrap()).unwrap();
        assert_eq!(decoded.event.facts.len(), 5);
        assert!(
            matches!(&decoded.event.facts[3], CanonicalEvent::Usage { usage } if usage.input_tokens == Some(10) && usage.output_tokens == Some(2))
        );
        assert!(
            matches!(&decoded.event.wire.unwrap().payload, WirePayload::RawJson{body} if std::str::from_utf8(body).unwrap().contains("Hello"))
        );
    }
}
