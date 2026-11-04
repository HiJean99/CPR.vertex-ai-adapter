//! CPR 原生 Management v2 页面与 Google gcloud ADC 授权流程。

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::management::{
        ManagementPage, ManagementRegistration, ManagementRequest, ManagementResource,
        ManagementResponse, ManagementRoute,
    },
    client::{TypedCall, TypedReply},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    accounts,
    auth::{self, SharedLogin},
    config::{Config, ProjectSelection},
    google,
};

pub fn registration() -> ManagementRegistration {
    ManagementRegistration {
        routes: vec![
            route("GET", "api/status"),
            route("POST", "api/login/start"),
            route("POST", "api/login/complete"),
            route("POST", "api/login/cancel"),
            route("POST", "api/project/select"),
            route("POST", "api/disconnect"),
        ],
        resources: vec![
            ManagementResource {
                path: "web/index.html".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "web/app.js".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "web/app.css".to_owned(),
                public: false,
            },
        ],
        pages: vec![ManagementPage {
            id: "vertex-ai".to_owned(),
            title: "Vertex AI".to_owned(),
            description: Some("Connect Google Cloud and use Gemini through Vertex AI".to_owned()),
            entry: "web/index.html".to_owned(),
            icon: None,
        }],
        callbacks: vec![],
    }
}

fn route(method: &str, path: &str) -> ManagementRoute {
    ManagementRoute {
        method: method.to_owned(),
        path: path.to_owned(),
        request_content_types: if method == "POST" {
            vec!["application/json".to_owned()]
        } else {
            vec![]
        },
        response_content_types: vec!["application/json".to_owned()],
    }
}

pub async fn handle(
    config: &Config,
    login: &SharedLogin,
    call: TypedCall<ManagementRequest>,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let result = match (call.request.method.as_str(), call.request.path.as_str()) {
        ("GET", "api/status") => status(config, &call.host).await,
        ("POST", "api/login/start") => start_login(config, login).await,
        ("POST", "api/login/complete") => complete_login(config, login, &call).await,
        ("POST", "api/login/cancel") => cancel_login(login, &call).await,
        ("POST", "api/project/select") => select_project(config, &call).await,
        ("POST", "api/disconnect") => disconnect(config, login, &call).await,
        _ => Err((404, "Unknown Vertex AI management route.".to_owned())),
    };
    match result {
        Ok(value) => json_reply(200, &value),
        Err((status, message)) => json_reply(status, &json!({"ok": false, "error": message})),
    }
}

async fn status(
    config: &Config,
    host: &gateway_plugin_sdk::client::HostClient,
) -> Result<Value, (u16, String)> {
    let gcloud_installed = auth::gcloud_available(config).await;
    let connected = config.adc_file().exists();
    let selection = read_selection(config).await;
    let group = accounts::ensure_group(host)
        .await
        .map_err(as_management_error)?;
    let mut email = selection.as_ref().map(|p| p.email.clone());
    let mut projects = Vec::new();
    let mut project_error = None;
    if connected && gcloud_installed {
        match auth::access_token(config).await {
            Ok(token) => {
                if email.is_none() {
                    email = google::user_info(host, &token)
                        .await
                        .ok()
                        .map(|identity| identity.email);
                }
                match google::list_projects(host, &token).await {
                    Ok(found) => projects = found,
                    Err(_) => project_error = Some("Could not list projects. Grant resourcemanager.projects.list to this Google account.".to_owned()),
                }
            }
            Err(_) => {
                project_error = Some(
                    "Google ADC could not refresh its token. Reconnect the Google account."
                        .to_owned(),
                )
            }
        }
    }
    let account = accounts::find_managed_account(host)
        .await
        .map_err(as_management_error)?;
    Ok(json!({
        "ok": true,
        "gcloudInstalled": gcloud_installed,
        "connected": connected,
        "email": email,
        "project": selection,
        "projects": projects,
        "projectError": project_error,
        "accountReady": account.as_ref().is_some_and(|a| a.enabled),
        "accountGroup": group.name,
        "accountGroupId": group.id,
        "billingCreditsUrl": selection.as_ref().map(|p| format!("https://console.cloud.google.com/billing?project={}", p.project_id)),
        "creditsNote": "Promotional credit balances are not exposed by the Cloud Billing API. Open Google Cloud Billing to view credits and expiry."
    }))
}

async fn start_login(config: &Config, login: &SharedLogin) -> Result<Value, (u16, String)> {
    if !auth::gcloud_available(config).await {
        return Err((503, "Google Cloud CLI is not installed in the CPR runtime. The adapter package expects the bundled Cloud SDK under its persistent data directory.".to_owned()));
    }
    let result = auth::start_login(config, login)
        .await
        .map_err(as_management_error)?;
    Ok(
        json!({"ok": true, "sessionId": result.session_id, "authorizationUrl": result.authorization_url, "expiresInSeconds": result.expires_in_seconds}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginCompleteRequest {
    session_id: String,
    authorization_code: String,
}

async fn complete_login(
    config: &Config,
    login: &SharedLogin,
    call: &TypedCall<ManagementRequest>,
) -> Result<Value, (u16, String)> {
    let input: LoginCompleteRequest = parse_body(&call.payload)?;
    let completed =
        auth::complete_login(config, login, &input.session_id, &input.authorization_code)
            .await
            .map_err(as_management_error)?;
    let identity = match google::user_info(&call.host, &completed.access_token).await {
        Ok(identity) => identity,
        Err(_) => {
            let _ = tokio::fs::remove_dir_all(&completed.pending_dir).await;
            return Err((
                502,
                "Google authorization succeeded, but the account identity could not be verified."
                    .to_owned(),
            ));
        }
    };
    auth::commit_login(config, &completed.pending_dir)
        .await
        .map_err(as_management_error)?;
    let (projects, project_error) = match google::list_projects(&call.host, &completed.access_token)
        .await
    {
        Ok(projects) => (projects, None),
        Err(_) => (
            Vec::new(),
            Some(
                "Could not list projects. Grant resourcemanager.projects.list to this Google account.",
            ),
        ),
    };
    Ok(
        json!({"ok": true, "email": identity.email, "projects": projects, "projectError": project_error, "message": "Google account connected. Select the Google Cloud project to use with Vertex AI."}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoginCancelRequest {
    session_id: Option<String>,
}

async fn cancel_login(
    login: &SharedLogin,
    call: &TypedCall<ManagementRequest>,
) -> Result<Value, (u16, String)> {
    let input: LoginCancelRequest = if call.payload.is_empty() {
        LoginCancelRequest { session_id: None }
    } else {
        parse_body(&call.payload)?
    };
    auth::cancel_login(login, input.session_id.as_deref())
        .await
        .map_err(as_management_error)?;
    Ok(json!({"ok": true}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectSelectRequest {
    project_id: String,
    #[serde(default = "default_location")]
    location: String,
}

fn default_location() -> String {
    "global".to_owned()
}

async fn select_project(
    config: &Config,
    call: &TypedCall<ManagementRequest>,
) -> Result<Value, (u16, String)> {
    let input: ProjectSelectRequest = parse_body(&call.payload)?;
    if !crate::config::valid_project_id(&input.project_id)
        || !crate::config::valid_location(&input.location)
    {
        return Err((
            400,
            "Invalid Google Cloud project ID or Vertex location.".to_owned(),
        ));
    }
    let token = auth::access_token(config)
        .await
        .map_err(as_management_error)?;
    let identity = google::user_info(&call.host, &token)
        .await
        .map_err(as_management_error)?;
    let projects = google::list_projects(&call.host, &token)
        .await
        .map_err(as_management_error)?;
    if !projects
        .iter()
        .any(|project| project.project_id == input.project_id)
    {
        return Err((403, "This Google account cannot list the selected project. Check project membership and Resource Manager permissions.".to_owned()));
    }
    let check = google::check_project(&call.host, &token, &input.project_id)
        .await
        .map_err(as_management_error)?;
    if !check.vertex_enabled {
        return Err((
            409,
            format!(
                "Vertex AI API is not enabled for {}. Enable it in Google Cloud Console, then retry: https://console.cloud.google.com/apis/library/aiplatform.googleapis.com?project={}",
                input.project_id, input.project_id
            ),
        ));
    }
    if check.billing_enabled == Some(false) {
        return Err((
            409,
            format!(
                "Google Cloud Billing is not linked to {}. Link a billing account before using Vertex AI: https://console.cloud.google.com/billing/linkedaccount?project={}",
                input.project_id, input.project_id
            ),
        ));
    }

    let group = accounts::ensure_group(&call.host)
        .await
        .map_err(as_management_error)?;
    let account_id = accounts::connect_project(&call.host, &identity.email)
        .await
        .map_err(as_management_error)?;
    let selection = ProjectSelection {
        project_id: input.project_id.clone(),
        location: input.location.clone(),
        email: identity.email.clone(),
        account_id: account_id.clone(),
        billing_enabled: check.billing_enabled,
        billing_account: check.billing_account.clone(),
        selected_at_ms: chrono_millis(),
    };
    if write_selection(config, &selection).await.is_err() {
        let _ = accounts::disconnect_project(&call.host, &account_id).await;
        return Err((
            500,
            "Vertex project selection could not be saved. The managed account was rolled back."
                .to_owned(),
        ));
    }
    Ok(json!({
        "ok": true, "email": identity.email, "project": selection,
        "accountGroup": group.name,
        "message": "Vertex AI is connected. Bind the Vertex AI account group to this adapter in CPR and create a Client Key for your application.",
        "billingWarning": if check.billing_enabled.is_none() { Some("Billing linkage could not be verified with the current Google permissions. Confirm it in Cloud Billing before sending paid requests.") } else { None }
    }))
}

async fn disconnect(
    config: &Config,
    login: &SharedLogin,
    call: &TypedCall<ManagementRequest>,
) -> Result<Value, (u16, String)> {
    if let Some(account) = accounts::find_managed_account(&call.host)
        .await
        .map_err(as_management_error)?
    {
        accounts::disconnect_project(&call.host, &account.account_id)
            .await
            .map_err(as_management_error)?;
    }
    auth::disconnect(config, login)
        .await
        .map_err(as_management_error)?;
    if tokio::fs::symlink_metadata(config.project_file())
        .await
        .is_ok()
    {
        tokio::fs::remove_file(config.project_file())
            .await
            .map_err(|_| {
                (
                    500,
                    "Vertex project selection could not be removed.".to_owned(),
                )
            })?;
    }
    Ok(
        json!({"ok": true, "message": "Google ADC disconnected. The Vertex account was removed from the Vertex AI group."}),
    )
}

async fn read_selection(config: &Config) -> Option<ProjectSelection> {
    let metadata = tokio::fs::symlink_metadata(config.project_file())
        .await
        .ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 16 * 1024 {
        return None;
    }
    let bytes = tokio::fs::read(config.project_file()).await.ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn write_selection(config: &Config, selection: &ProjectSelection) -> Result<(), ()> {
    use std::os::unix::fs::PermissionsExt;
    let root = config.data_dir();
    tokio::fs::create_dir_all(&root).await.map_err(|_| ())?;
    let path = config.project_file();
    let temp = root.join(format!(".project-{}.tmp", uuid::Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec(selection).map_err(|_| ())?;
    tokio::fs::write(&temp, bytes).await.map_err(|_| ())?;
    tokio::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|_| ())?;
    tokio::fs::rename(&temp, &path).await.map_err(|_| ())?;
    Ok(())
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, (u16, String)> {
    if body.len() > 64 * 1024 {
        return Err((413, "Request body is too large.".to_owned()));
    }
    serde_json::from_slice(body).map_err(|_| (400, "Invalid JSON request body.".to_owned()))
}

fn json_reply(status: u16, value: &Value) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let body = serde_json::to_vec(value)
        .map_err(|_| PluginFault::new(ErrorCode::Fault, "Could not encode management response."))?;
    let response = ManagementResponse {
        status,
        content_type: "application/json".to_owned(),
        headers: vec![],
    };
    Ok(TypedReply::new(response).with_payload(body))
}

fn as_management_error(error: PluginFault) -> (u16, String) {
    let message = error.message;
    let status = match error.code {
        ErrorCode::Conflict => 409,
        ErrorCode::InvalidInput => 400,
        ErrorCode::PermissionDenied => 403,
        ErrorCode::Capacity => 429,
        _ => 500,
    };
    (status, message)
}

fn chrono_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
