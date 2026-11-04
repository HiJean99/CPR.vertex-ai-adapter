//! CPR 插件进程入口：Google gcloud ADC + Vertex Gemini adapter + 管理页面。

mod accounts;
mod auth;
mod config;
mod execute;
mod google;
mod management;
mod translate;

use config::Config;
use gateway_plugin_sdk::{
    Manifest,
    client::{PluginBuilder, PluginSession, SessionConfig, SessionError, TypedReply, methods},
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let session = PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await?;
    let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json"))?;
    if session.handshake().plugin_id != "mingli42.vertex-ai-adapter"
        || session.handshake().contributes != manifest.contributes
    {
        return Err(SessionError::Handshake.into());
    }
    let config: Config = serde_json::from_value(session.handshake().configuration.clone())?;
    config.validate().map_err(|_| SessionError::Handshake)?;
    auth::cleanup_stale_pending(&config)
        .await
        .map_err(|error| std::io::Error::other(error.message))?;
    let login = auth::shared_login();
    let management_config = config.clone();
    let management_login = login.clone();

    let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .on(methods::UPSTREAM_ADAPTER_REGISTER, move |call| {
            async move {
                // CPR's static model catalog requires a real account ID at
                // registration time. Create only the managed placeholder
                // account here; account groups belong to the management page.
                let account_id = accounts::ensure_managed_account(&call.host).await?;
                Ok(TypedReply::new(Config::adapter_registration(&account_id)))
            }
        })?
        .model_catalog(Config::catalog_registration())?
        .on(methods::UPSTREAM_ADAPTER_EXECUTE, move |call| {
            let config = config.clone();
            async move { execute::run(&config, call).await }
        })?
        .management(management::registration(), move |call| {
            let config = management_config.clone();
            let login = management_login.clone();
            async move { management::handle(&config, &login, call).await }
        })?
        .build()?;
    session.run(plugin).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_identity_and_management_resources_are_consistent() {
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        assert_eq!(manifest.plugin_id().unwrap(), "mingli42.vertex-ai-adapter");
        assert_eq!(manifest.contributes.len(), 3);
        let registration = management::registration();
        assert_eq!(registration.pages.len(), 1);
        assert!(
            registration
                .resources
                .iter()
                .all(|resource| manifest.resources.contains_key(&resource.path))
        );
    }
}
