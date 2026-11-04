//! CPR 受管 Vertex SSE 请求。下游 JSON 在完整读取后发出终态事件，SSE 实时转发。

use std::{num::NonZeroUsize, time::Duration};

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        model::{ExecutionEvent, facts::ResponseObservation},
        upstream_adapter::{
            BuiltinProvider, UpstreamAdapterEvent, UpstreamAdapterRequest, UpstreamFailure,
            UpstreamFailureKind, UpstreamHttpRequest,
        },
    },
    client::{Empty, ResponseStream, StreamSender, TypedCall, TypedReply},
};
use serde_json::Value;
use tokio::time::timeout;

use crate::{
    accounts, auth,
    config::{self, Config, ProjectSelection},
    translate,
};

const MAX_SSE_BUFFER_BYTES: usize = 2 * 1024 * 1024;

fn response_observation() -> ResponseObservation {
    // 观测传输必须与注册声明一致；非流式请求只是同一适配器的另一种响应编码
    ResponseObservation::new(config::UPSTREAM_TRANSPORT.as_str())
}

pub async fn run(
    config: &Config,
    call: TypedCall<UpstreamAdapterRequest>,
) -> Result<TypedReply<Empty>, PluginFault> {
    if call.request.adapter_id != config::ADAPTER_ID
        || !matches!(call.request.provider, BuiltinProvider::OpenAi)
        || call.request.protocol != "openai"
    {
        return Err(invalid(
            "Vertex adapter provider, protocol, or identity mismatch.",
        ));
    }
    let model = Config::upstream_model(&call.request.upstream_model).ok_or_else(|| {
        invalid("The requested Gemini model is not in this adapter's model catalog.")
    })?;
    let selection = read_selection(config).await?;
    if selection.account_id != call.request.account_id {
        return Err(invalid(
            "The selected CPR account does not match the connected Vertex project.",
        ));
    }
    let account = accounts::get_account(&call.host, &selection.account_id).await?;
    if account.facts.upstream_account_id.as_deref() != Some(config::ACCOUNT_MARKER)
        || account.facts.authentication_kind != "api_key"
    {
        return Err(invalid(
            "The selected CPR account is not owned by Vertex AI Adapter.",
        ));
    }
    let token = auth::access_token(config).await?;
    let (body, downstream_streaming) = translate::request_to_vertex(&call.payload)?;
    let path = Config::project_path(&selection.project_id, &selection.location, model)?;
    let request = UpstreamHttpRequest {
        method: "POST".to_owned(),
        path,
        query: vec![("alt".to_owned(), "sse".to_owned())],
        headers: vec![
            (
                "authorization".to_owned(),
                format!("Bearer {token}").into_bytes(),
            ),
            (
                "x-goog-user-project".to_owned(),
                selection.project_id.as_bytes().to_vec(),
            ),
            ("content-type".to_owned(), b"application/json".to_vec()),
            ("accept".to_owned(), b"text/event-stream".to_vec()),
        ],
    };
    let response = call.host.upstream_http(request, body).await?;
    let status = response.status;
    let http_version = response.http_version.clone();
    if !(200..300).contains(&status) {
        let raw = response.body.collect(2 * 1024 * 1024).await?;
        let mut event = upstream_error(&raw, status);
        let mut observation = response_observation();
        observation.http_version = Some(http_version);
        observation.status_code = Some(status);
        event.observation = Some(observation);
        return single_event(event).await;
    }

    let ids = translate::StreamIds::new(&call.request.upstream_model);
    let (sender, stream) = ResponseStream::channel(NonZeroUsize::new(4).expect("nonzero channel"));
    let mut observation = response_observation();
    observation.http_version = Some(http_version);
    observation.status_code = Some(status);
    if downstream_streaming {
        let mut starts = translate::stream_start_events(&ids);
        if let Some(first) = starts.first_mut() {
            first.observation = Some(observation);
        }
        for event in starts {
            send_event(&sender, event).await?;
        }
        tokio::spawn(stream_response(response.body, sender, ids, true, None));
    } else {
        tokio::spawn(stream_response(
            response.body,
            sender,
            ids,
            false,
            Some(observation),
        ));
    }
    Ok(TypedReply::new(Empty {}).with_stream(stream))
}

async fn read_selection(config: &Config) -> Result<ProjectSelection, PluginFault> {
    let path = config.project_file();
    let metadata = tokio::fs::symlink_metadata(&path).await.map_err(|_| {
        invalid("Connect a Google Cloud project in the Vertex AI management page first.")
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(invalid("Vertex project selection is invalid."));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| invalid("Vertex project selection is unavailable."))?;
    let selection: ProjectSelection = serde_json::from_slice(&bytes)
        .map_err(|_| invalid("Vertex project selection is invalid."))?;
    if !config::valid_project_id(&selection.project_id)
        || !config::valid_location(&selection.location)
        || selection.account_id.is_empty()
    {
        return Err(invalid("Vertex project selection contains invalid values."));
    }
    Ok(selection)
}

async fn single_event(event: UpstreamAdapterEvent) -> Result<TypedReply<Empty>, PluginFault> {
    let payload = event
        .encode()
        .map_err(|_| invalid("Vertex response exceeded the CPR event limit."))?;
    Ok(TypedReply::new(Empty {}).with_stream(ResponseStream::from_chunks(vec![payload])))
}

async fn send_event(sender: &StreamSender, event: UpstreamAdapterEvent) -> Result<(), PluginFault> {
    let payload = event
        .encode()
        .map_err(|_| invalid("Vertex stream event exceeded the CPR event limit."))?;
    sender.send(payload).await.map_err(|_| {
        PluginFault::new(
            ErrorCode::Cancelled,
            "Vertex response stream was cancelled.",
        )
    })
}

async fn stream_response(
    mut body: gateway_plugin_sdk::client::HostHttpBody,
    sender: StreamSender,
    ids: translate::StreamIds,
    downstream_streaming: bool,
    observation: Option<ResponseObservation>,
) {
    let mut decoder = SseDecoder::default();
    let mut sequence = 3_u64;
    let mut full_text = String::new();
    let mut usage = gateway_plugin_sdk::call::model::Usage::default();
    let mut finish_reason = None;
    loop {
        let chunk = match timeout(Duration::from_secs(120), body.read()).await {
            Ok(Ok(Some(bytes))) => bytes,
            Ok(Ok(None)) => break,
            _ => {
                let _ = sender
                    .fail(PluginFault::new(
                        ErrorCode::Upstream,
                        "Vertex SSE stream ended unexpectedly.",
                    ))
                    .await;
                return;
            }
        };
        let frames = match decoder.push(&chunk) {
            Ok(frames) => frames,
            Err(error) => {
                let _ = sender.fail(error).await;
                return;
            }
        };
        for frame in frames {
            let Some(data) = sse_data(&frame) else {
                continue;
            };
            if data == "[DONE]" {
                continue;
            }
            let delta = match translate::stream_delta(data.as_bytes()) {
                Ok(delta) => delta,
                Err(error) => {
                    let _ = sender.fail(error).await;
                    return;
                }
            };
            if !delta.text.is_empty() {
                full_text.push_str(&delta.text);
                if downstream_streaming {
                    if send_event(
                        &sender,
                        translate::stream_text_event(&ids, sequence, &delta.text),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    sequence += 1;
                }
            }
            if let Some(new_usage) = delta.usage {
                usage = new_usage;
            }
            if delta.finish_reason.is_some() {
                finish_reason = delta.finish_reason;
            }
        }
    }
    if let Ok(Some(frame)) = decoder.finish()
        && let Some(data) = sse_data(&frame)
        && data != "[DONE]"
    {
        match translate::stream_delta(data.as_bytes()) {
            Ok(delta) => {
                if !delta.text.is_empty() {
                    full_text.push_str(&delta.text);
                    if downstream_streaming {
                        if send_event(
                            &sender,
                            translate::stream_text_event(&ids, sequence, &delta.text),
                        )
                        .await
                        .is_err()
                        {
                            return;
                        }
                        sequence += 1;
                    }
                }
                if let Some(new_usage) = delta.usage {
                    usage = new_usage;
                }
                if delta.finish_reason.is_some() {
                    finish_reason = delta.finish_reason;
                }
            }
            Err(error) => {
                let _ = sender.fail(error).await;
                return;
            }
        }
    }
    if full_text.is_empty() {
        let _ = sender
            .fail(invalid("Vertex returned an empty text stream."))
            .await;
        return;
    }
    let reason = finish_reason.unwrap_or(gateway_plugin_sdk::call::model::FinishReason::Stop);
    if downstream_streaming {
        for event in translate::stream_complete_events(&ids, sequence, &full_text, usage, reason) {
            if send_event(&sender, event).await.is_err() {
                return;
            }
        }
    } else {
        let mut event = translate::completed_response_event(&ids, &full_text, usage, reason);
        event.observation = observation;
        let _ = send_event(&sender, event).await;
    }
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, PluginFault> {
        if self.buffer.len().saturating_add(chunk.len()) > MAX_SSE_BUFFER_BYTES {
            return Err(PluginFault::new(
                ErrorCode::Capacity,
                "Vertex SSE frame exceeded the buffer limit.",
            ));
        }
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some((index, delimiter_len)) = find_sse_delimiter(&self.buffer) {
            let frame = self
                .buffer
                .drain(..index + delimiter_len)
                .collect::<Vec<_>>();
            frames.push(frame);
        }
        Ok(frames)
    }

    fn finish(&mut self) -> Result<Option<Vec<u8>>, PluginFault> {
        if self.buffer.is_empty() {
            return Ok(None);
        }
        if self.buffer.len() > MAX_SSE_BUFFER_BYTES {
            return Err(PluginFault::new(
                ErrorCode::Capacity,
                "Vertex SSE frame exceeded the buffer limit.",
            ));
        }
        Ok(Some(std::mem::take(&mut self.buffer)))
    }
}

fn find_sse_delimiter(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| (index, 2));
    let crlf = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn sse_data(frame: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(frame).ok()?;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

fn upstream_error(raw: &[u8], status: u16) -> UpstreamAdapterEvent {
    let source: Value = serde_json::from_slice(raw).unwrap_or(Value::Null);
    let error = source.get("error");
    let kind = match status {
        400 | 422 => UpstreamFailureKind::InvalidRequest,
        401 => UpstreamFailureKind::Unauthorized,
        403 => UpstreamFailureKind::PermissionDenied,
        404 => UpstreamFailureKind::Unsupported,
        429 => UpstreamFailureKind::RateLimited,
        408 | 504 => UpstreamFailureKind::Timeout,
        500..=599 => UpstreamFailureKind::Unavailable,
        _ => UpstreamFailureKind::Protocol,
    };
    let upstream_message = error
        .and_then(|item| item.get("message"))
        .and_then(Value::as_str);
    let code = error
        .and_then(|item| item.get("status"))
        .and_then(Value::as_str);
    let mut result = UpstreamAdapterEvent::new(ExecutionEvent::default());
    result.failure = Some(UpstreamFailure {
        kind,
        error_type: None,
        status: Some(status),
        retry_after_ms: None,
        message: upstream_message
            .unwrap_or("Vertex AI request failed.")
            .chars()
            .take(1024)
            .collect(),
        code: code.map(str::to_owned),
    });
    result
}

fn invalid(message: &'static str) -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_observation_matches_registered_adapter_transport() {
        let adapter = Config::adapter_registration("acct_vertex_test")
            .adapters
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(response_observation().transport, adapter.transport.as_str());
    }

    #[test]
    fn decoder_handles_chunk_boundaries_and_crlf() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(b"data: {\"x\":").unwrap().is_empty());
        let frames = decoder.push(b"1}\r\n\r\n").unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(sse_data(&frames[0]).as_deref(), Some("{\"x\":1}"));
    }

    #[test]
    fn rate_limit_is_classified_as_upstream_failure() {
        let raw =
            br#"{"error":{"code":429,"message":"Quota limited","status":"RESOURCE_EXHAUSTED"}}"#;
        let event = upstream_error(raw, 429);
        let failure = event.failure.unwrap();
        assert!(matches!(failure.kind, UpstreamFailureKind::RateLimited));
        assert_eq!(failure.code.as_deref(), Some("RESOURCE_EXHAUSTED"));
    }
}
