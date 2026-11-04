//! CPR 自有 Vertex 账号与账号组。API Key 只是 OpenAI Provider 的适配器占位凭据；
//! 真正的 Google 身份始终保存在 gcloud ADC，不把 Google Token 写入 CPR 账号。

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        host::{
            AuthCredential, AuthGetRequest, AuthListRequest, AuthListResult, AuthRuntimeAccount,
            AuthSaveRequest, AuthSaveResult, CredentialFacts,
        },
        resources::{GroupEnsureRequest, GroupMembersChange, ManagedResource},
    },
    client::HostClient,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::config::{ACCOUNT_MARKER, GROUP_NAME, GROUP_RESOURCE_KEY};

pub async fn ensure_group(host: &HostClient) -> Result<ManagedResource, PluginFault> {
    host.ensure_group(GroupEnsureRequest {
        resource_key: GROUP_RESOURCE_KEY.to_owned(),
        name: GROUP_NAME.to_owned(),
        color: "#4285F4FF".to_owned(),
        description: Some("Gemini accounts connected through CPR.vertex-ai-adapter".to_owned()),
    })
    .await
}

pub async fn ensure_managed_account(host: &HostClient) -> Result<String, PluginFault> {
    if let Some(account) = find_managed_account(host).await? {
        if account.provider_id != "openai"
            || account.authentication_kind != "api_key"
            || !account.enabled
        {
            return Err(PluginFault::new(
                ErrorCode::Rejected,
                "The managed Vertex CPR account is disabled or has an unexpected provider.",
            ));
        }
        return Ok(account.account_id);
    }

    let facts = credential_facts("", None)?;
    let saved: AuthSaveResult = payload_call(
        host,
        "host.auth.save",
        AuthSaveRequest::Create {
            provider_id: "openai".to_owned(),
            facts,
        },
    )
    .await?;
    Ok(saved.account_id)
}

pub async fn find_managed_account(
    host: &HostClient,
) -> Result<Option<AuthRuntimeAccount>, PluginFault> {
    let mut cursor = None;
    for _ in 0..20 {
        let page: AuthListResult = payload_call(
            host,
            "host.auth.list",
            AuthListRequest {
                provider_id: Some("openai".to_owned()),
                cursor: cursor.clone(),
                limit: 200,
            },
        )
        .await?;
        if let Some(account) = page
            .accounts
            .into_iter()
            .find(|account| account.upstream_account_id.as_deref() == Some(ACCOUNT_MARKER))
        {
            return Ok(Some(account));
        }
        let Some(next) = page.next_cursor else {
            return Ok(None);
        };
        cursor = Some(next);
    }
    Err(PluginFault::new(
        ErrorCode::Capacity,
        "Too many OpenAI accounts to locate the Vertex account safely.",
    ))
}

pub async fn get_account(
    host: &HostClient,
    account_id: &str,
) -> Result<AuthCredential, PluginFault> {
    payload_call(
        host,
        "host.auth.get",
        AuthGetRequest {
            account_id: account_id.to_owned(),
        },
    )
    .await
}

pub async fn connect_project(host: &HostClient, email: &str) -> Result<String, PluginFault> {
    let existing = find_managed_account(host).await?;
    let facts = match &existing {
        Some(runtime) => {
            if runtime.provider_id != "openai"
                || runtime.authentication_kind != "api_key"
                || !runtime.enabled
            {
                return Err(PluginFault::new(
                    ErrorCode::Rejected,
                    "The managed Vertex CPR account is disabled or has an unexpected provider.",
                ));
            }
            let current = get_account(host, &runtime.account_id).await?;
            if current.facts.upstream_account_id.as_deref() != Some(ACCOUNT_MARKER) {
                return Err(PluginFault::new(
                    ErrorCode::Rejected,
                    "Vertex account ownership check failed.",
                ));
            }
            credential_facts(email, Some(&current.facts))?
        }
        None => credential_facts(email, None)?,
    };

    let account_id = if let Some(runtime) = existing {
        let _: AuthSaveResult = payload_call(
            host,
            "host.auth.save",
            AuthSaveRequest::Replace {
                account_id: runtime.account_id.clone(),
                credential_revision: runtime.credential_revision,
                facts,
            },
        )
        .await?;
        runtime.account_id
    } else {
        let saved: AuthSaveResult = payload_call(
            host,
            "host.auth.save",
            AuthSaveRequest::Create {
                provider_id: "openai".to_owned(),
                facts,
            },
        )
        .await?;
        saved.account_id
    };

    host.change_group_members(GroupMembersChange {
        resource_key: GROUP_RESOURCE_KEY.to_owned(),
        add: vec![account_id.clone()],
        remove: vec![],
    })
    .await?;
    Ok(account_id)
}

pub async fn disconnect_project(host: &HostClient, account_id: &str) -> Result<(), PluginFault> {
    let runtime = find_managed_account(host).await?.ok_or_else(|| {
        PluginFault::new(
            ErrorCode::Rejected,
            "The managed Vertex account does not exist.",
        )
    })?;
    if runtime.account_id != account_id
        || runtime.upstream_account_id.as_deref() != Some(ACCOUNT_MARKER)
    {
        return Err(PluginFault::new(
            ErrorCode::Rejected,
            "Vertex account ownership check failed.",
        ));
    }
    host.change_group_members(GroupMembersChange {
        resource_key: GROUP_RESOURCE_KEY.to_owned(),
        add: vec![],
        remove: vec![account_id.to_owned()],
    })
    .await?;
    let current = get_account(host, account_id).await?;
    let facts = credential_facts("", Some(&current.facts))?;
    let _: AuthSaveResult = payload_call(
        host,
        "host.auth.save",
        AuthSaveRequest::Replace {
            account_id: account_id.to_owned(),
            credential_revision: current.credential_revision,
            facts: CredentialFacts {
                name: "Vertex AI (not connected)".to_owned(),
                email: None,
                plan_type: None,
                ..facts
            },
        },
    )
    .await?;
    Ok(())
}

fn credential_facts(
    email: &str,
    current: Option<&CredentialFacts>,
) -> Result<CredentialFacts, PluginFault> {
    let installation_id = current
        .and_then(|facts| facts.material.get("installation_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let material: Map<String, Value> = json!({
        "schema_version": 1,
        "installation_id": installation_id,
        "api_key": "vertex-adapter-managed-placeholder",
        "base_url": "https://aiplatform.googleapis.com/v1/"
    })
    .as_object()
    .cloned()
    .ok_or_else(invalid)?;
    Ok(CredentialFacts {
        name: if email.is_empty() {
            "Vertex AI (not connected)".to_owned()
        } else {
            format!("Vertex AI · {email}")
        },
        authentication_kind: "api_key".to_owned(),
        material,
        email: if email.is_empty() {
            None
        } else {
            Some(email.to_owned())
        },
        upstream_user_id: None,
        upstream_account_id: Some(ACCOUNT_MARKER.to_owned()),
        plan_type: if email.is_empty() {
            None
        } else {
            Some("vertex_ai".to_owned())
        },
        has_refresh_token: false,
        access_token_expires_at_ms: None,
        next_refresh_at_ms: None,
    })
}

async fn payload_call<T: Serialize, R: DeserializeOwned>(
    host: &HostClient,
    method: &str,
    request: T,
) -> Result<R, PluginFault> {
    let payload = serde_json::to_vec(&request).map_err(|_| invalid())?;
    let reply = host
        .call(method, json!({}), payload)
        .await
        .map_err(gateway_plugin_sdk::client::SessionError::into_plugin_fault)?;
    if reply.result != json!({}) {
        return Err(invalid());
    }
    serde_json::from_slice(&reply.payload).map_err(|_| invalid())
}

fn invalid() -> PluginFault {
    PluginFault::new(
        ErrorCode::InvalidInput,
        "Invalid CPR account callback payload.",
    )
}
