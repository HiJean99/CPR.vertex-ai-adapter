//! 从运行环境 ADC 获取短期 Google Token，不记录或持久化令牌

use std::sync::Arc;

use gateway_plugin_sdk::{ErrorCode, PluginFault};
use gcp_auth::TokenProvider;
use tokio::sync::OnceCell;

static TOKEN_PROVIDER: OnceCell<Arc<dyn TokenProvider>> = OnceCell::const_new();

pub async fn access_token() -> Result<String, PluginFault> {
    let provider = TOKEN_PROVIDER
        .get_or_try_init(|| async {
            gcp_auth::provider().await.map_err(|_| {
                PluginFault::new(
                    ErrorCode::Rejected,
                    "Google application default credentials are unavailable.",
                )
            })
        })
        .await?;
    let token = provider
        .token(&["https://www.googleapis.com/auth/cloud-platform"])
        .await
        .map_err(|_| PluginFault::new(ErrorCode::Rejected, "Google ADC token refresh failed."))?;
    Ok(token.as_str().to_owned())
}
