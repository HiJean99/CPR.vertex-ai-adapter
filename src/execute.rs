//! 使用 CPR 受管 HTTP 调用 Vertex，返回原生费用及发送状态可识别的模型事实

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        model::ExecutionEvent,
        upstream_adapter::{
            BuiltinProvider, UpstreamAdapterEvent, UpstreamAdapterRequest, UpstreamFailure,
            UpstreamFailureKind, UpstreamHttpRequest,
        },
    },
    client::{Empty, ResponseStream, TypedCall, TypedReply},
};
use serde_json::Value;

use crate::{auth, config::Config, translate};

pub async fn run(
    config: &Config,
    call: TypedCall<UpstreamAdapterRequest>,
) -> Result<TypedReply<Empty>, PluginFault> {
    let public_model = &call.request.upstream_model;
    if !matches!(call.request.provider, BuiltinProvider::OpenAi)
        || call.request.protocol != "openai"
        || call.request.account_id != config.account_id
    {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "Vertex adapter account or protocol mismatch.",
        ));
    }
    let model = config.upstream_model(public_model).ok_or_else(|| {
        PluginFault::new(ErrorCode::InvalidInput, "Vertex model was not configured.")
    })?;
    let body = translate::request_to_vertex(&call.payload)?;
    let token = auth::access_token().await?;
    let response = call
        .host
        .upstream_http(
            UpstreamHttpRequest {
                method: "POST".to_owned(),
                path: format!("{model}:generateContent"),
                query: vec![],
                headers: vec![
                    (
                        "authorization".to_owned(),
                        format!("Bearer {token}").into_bytes(),
                    ),
                    (
                        "x-goog-user-project".to_owned(),
                        config.quota_project().as_bytes().to_vec(),
                    ),
                    ("content-type".to_owned(), b"application/json".to_vec()),
                    ("accept".to_owned(), b"application/json".to_vec()),
                ],
            },
            body,
        )
        .await?;
    let status = response.status;
    let http_version = response.http_version.clone();
    let raw = response.body.collect(8 * 1024 * 1024).await?;
    let mut event = if (200..300).contains(&status) {
        translate::response_to_event(&raw, public_model)?
    } else {
        upstream_error(&raw, status)
    };
    let mut observation =
        gateway_plugin_sdk::call::model::facts::ResponseObservation::new("http_json");
    observation.http_version = Some(http_version);
    observation.status_code = Some(status);
    event.observation = Some(observation);
    let payload = event.encode().map_err(|_| {
        PluginFault::new(
            ErrorCode::InvalidInput,
            "Vertex response exceeds the plugin event limit.",
        )
    })?;
    Ok(TypedReply::new(Empty {}).with_stream(ResponseStream::from_chunks(vec![payload])))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_429_is_retryable_classification_not_fake_success() {
        let raw =
            br#"{"error":{"code":429,"message":"Quota limited","status":"RESOURCE_EXHAUSTED"}}"#;
        let event = upstream_error(raw, 429);
        let failure = event.failure.unwrap();
        assert!(matches!(failure.kind, UpstreamFailureKind::RateLimited));
        assert_eq!(failure.code.as_deref(), Some("RESOURCE_EXHAUSTED"));
        assert!(event.event.facts.is_empty());
    }
}
