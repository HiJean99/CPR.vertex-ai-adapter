//! Google Cloud API 查询：用户身份、项目发现、Vertex API 与 Billing 状态。
//! 所有 HTTP 请求经 CPR HostClient，遵守宿主网络、代理和超时边界。

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::host::HttpRequest,
    client::{HostClient, HostHttpResponse},
};
use serde::Serialize;
use serde_json::Value;

use crate::config::valid_project_id;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleIdentity {
    pub email: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleProject {
    pub project_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectStatus {
    pub vertex_enabled: bool,
    pub billing_enabled: Option<bool>,
    pub billing_account: Option<String>,
}

pub async fn user_info(host: &HostClient, token: &str) -> Result<GoogleIdentity, PluginFault> {
    let value = get_json(
        host,
        "https://openidconnect.googleapis.com/v1/userinfo",
        token,
    )
    .await?;
    let email = value
        .get("email")
        .and_then(Value::as_str)
        .filter(|email| email.contains('@') && email.len() <= 320)
        .ok_or_else(|| {
            PluginFault::new(
                ErrorCode::Rejected,
                "Google did not return a usable account email.",
            )
        })?;
    Ok(GoogleIdentity {
        email: email.to_owned(),
    })
}

pub async fn list_projects(
    host: &HostClient,
    token: &str,
) -> Result<Vec<GoogleProject>, PluginFault> {
    let mut page_token: Option<String> = None;
    let mut result = Vec::new();
    for _ in 0..20 {
        let mut url =
            "https://cloudresourcemanager.googleapis.com/v1/projects?pageSize=1000".to_owned();
        if let Some(next) = &page_token {
            if !next
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err(PluginFault::new(
                    ErrorCode::Rejected,
                    "Google Resource Manager returned an invalid page token.",
                ));
            }
            url.push_str("&pageToken=");
            url.push_str(next);
        }
        let value = get_json(host, &url, token).await.map_err(|_| {
            PluginFault::new(ErrorCode::Rejected, "Could not list Google Cloud projects. The account needs resourcemanager.projects.list permission.")
        })?;
        let projects = value
            .get("projects")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                PluginFault::new(
                    ErrorCode::Rejected,
                    "Google Resource Manager returned an invalid project list.",
                )
            })?;
        for project in projects {
            if project.get("lifecycleState").and_then(Value::as_str) != Some("ACTIVE") {
                continue;
            }
            let Some(project_id) = project.get("projectId").and_then(Value::as_str) else {
                continue;
            };
            if !valid_project_id(project_id) {
                continue;
            }
            let name = project
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .unwrap_or(project_id);
            result.push(GoogleProject {
                project_id: project_id.to_owned(),
                name: name.to_owned(),
            });
        }
        page_token = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if page_token.is_none() {
            break;
        }
    }
    result.sort_by_key(|project| project.name.to_lowercase());
    Ok(result)
}

pub async fn check_project(
    host: &HostClient,
    token: &str,
    project_id: &str,
) -> Result<ProjectStatus, PluginFault> {
    if !valid_project_id(project_id) {
        return Err(PluginFault::new(
            ErrorCode::InvalidInput,
            "Invalid Google Cloud project ID.",
        ));
    }
    let service_url = format!(
        "https://serviceusage.googleapis.com/v1/projects/{project_id}/services/aiplatform.googleapis.com"
    );
    let service = get_json(host, &service_url, token).await.map_err(|_| {
        PluginFault::new(
            ErrorCode::Rejected,
            "Cannot inspect Vertex AI API status. Check Service Usage API access for this project.",
        )
    })?;
    let vertex_enabled = service.get("state").and_then(Value::as_str) == Some("ENABLED");

    let billing_url =
        format!("https://cloudbilling.googleapis.com/v1/projects/{project_id}/billingInfo");
    let (billing_enabled, billing_account) = match get_json(host, &billing_url, token).await {
        Ok(billing) => (
            billing.get("billingEnabled").and_then(Value::as_bool),
            billing
                .get("billingAccountName")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        // Billing visibility is optional; an authorization failure must not be
        // misreported as no billing or no promotional credits.
        Err(_) => (None, None),
    };
    Ok(ProjectStatus {
        vertex_enabled,
        billing_enabled,
        billing_account,
    })
}

async fn get_json(host: &HostClient, url: &str, token: &str) -> Result<Value, PluginFault> {
    let response = host
        .http(
            HttpRequest {
                method: "GET".to_owned(),
                url: url.to_owned(),
                headers: vec![
                    (
                        "authorization".to_owned(),
                        format!("Bearer {token}").into_bytes(),
                    ),
                    ("accept".to_owned(), b"application/json".to_vec()),
                ],
            },
            Vec::new(),
        )
        .await?;
    parse_json_response(response).await
}

async fn parse_json_response(response: HostHttpResponse) -> Result<Value, PluginFault> {
    let status = response.status;
    let body = response.body.collect(2 * 1024 * 1024).await?;
    if !(200..300).contains(&status) {
        return Err(PluginFault::new(
            ErrorCode::Rejected,
            "Google Cloud API request was rejected.",
        ));
    }
    serde_json::from_slice(&body).map_err(|_| {
        PluginFault::new(
            ErrorCode::Rejected,
            "Google Cloud API returned invalid JSON.",
        )
    })
}
