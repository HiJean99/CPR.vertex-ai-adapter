//! CPR 官方插件进程入口，注册仅限 Vertex AI 文本适配器

mod auth;
mod config;
mod execute;
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
    let adapter_registration = config.adapter_registration();
    let catalog_registration = config.catalog_registration();
    let plugin = PluginBuilder::from_json(include_bytes!("../plugin.json"))?
        .on(methods::UPSTREAM_ADAPTER_REGISTER, move |_| {
            let registration = adapter_registration.clone();
            async move { Ok(TypedReply::new(registration)) }
        })?
        .model_catalog(catalog_registration)?
        .on(methods::UPSTREAM_ADAPTER_EXECUTE, move |call| {
            let config = config.clone();
            async move { execute::run(&config, call).await }
        })?
        .build()?;
    session.run(plugin).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn author_manifest_matches_plugin_identity_and_contract() {
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        assert_eq!(manifest.plugin_id().unwrap(), "mingli42.vertex-ai-adapter");
        assert_eq!(manifest.contributes.len(), 2);
    }
}
