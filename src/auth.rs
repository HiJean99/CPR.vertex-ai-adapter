//! 使用 Google Cloud Rust 官方 ADC 实现，包括外部身份联合（WIF）。
//! 从不在配置、持久状态或响应中存放 Access Token。

use gateway_plugin_sdk::{ErrorCode, PluginFault};
use google_cloud_auth::credentials::{AccessTokenCredentials, Builder};
use tokio::sync::OnceCell;

static TOKEN_PROVIDER: OnceCell<AccessTokenCredentials> = OnceCell::const_new();

pub async fn access_token() -> Result<String, PluginFault> {
    let credentials = TOKEN_PROVIDER
        .get_or_try_init(|| async {
            Builder::default()
                .with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
                .build_access_token_credentials()
                .map_err(|_| {
                    PluginFault::new(
                        ErrorCode::Rejected,
                        "Google ADC configuration is unavailable or invalid.",
                    )
                })
        })
        .await?;
    let token = credentials
        .access_token()
        .await
        .map_err(|_| PluginFault::new(ErrorCode::Rejected, "Google ADC token refresh failed."))?;
    Ok(token.token)
}

#[cfg(test)]
mod tests {
    use google_cloud_auth::credentials::external_account;
    use serde_json::json;

    #[tokio::test]
    async fn accepts_wif_config_schema_without_fetching_credentials() {
        let config = json!({
            "type": "external_account",
            "audience": "//iam.googleapis.com/projects/123456789/locations/global/workloadIdentityPools/example/providers/oidc",
            "subject_token_type": "urn:ietf:params:oauth:token-type:jwt",
            "token_url": "https://sts.googleapis.com/v1/token",
            "credential_source": {"file": "/nonexistent/test-only-oidc-token.jwt"}
        });
        let result = external_account::Builder::new(config)
            .with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
            .build_access_token_credentials();
        assert!(result.is_ok());
    }
}
