//! CPR 3.38+ 兼容配置、固定 Gemini 文本模型目录和安全路径校验。

use std::path::PathBuf;

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        catalog::{AdaptedModel, ModelCatalogRegistrationV2, ModelPresentation},
        upstream_adapter::{
            BuiltinProvider, UpstreamAdapterDeclaration, UpstreamAdapterRegistration,
            UpstreamTransport,
        },
    },
};
use serde::{Deserialize, Serialize};

pub const ADAPTER_ID: &str = "mingli42.vertex-ai-adapter.vertex-gemini";
pub const MODEL_PREFIX: &str = "vertex/google/";
pub const ACCOUNT_MARKER: &str = "mingli42.vertex-ai-adapter";
pub const GROUP_RESOURCE_KEY: &str = "vertex-ai";
pub const GROUP_NAME: &str = "Vertex AI";

/// CPR 在发布时冻结模型目录，因此第一版使用经过公开上游实现交叉核对的
/// Gemini 文本模型 allowlist；不把未经发现的模型虚构成可用模型。
pub const GEMINI_MODELS: &[(&str, &str)] = &[
    ("gemini-3.8-flash", "Gemini 3.8 Flash"),
    ("gemini-3.1-pro-preview", "Gemini 3.1 Pro Preview"),
    ("gemini-2.5-flash", "Gemini 2.5 Flash"),
    ("gemini-2.5-pro", "Gemini 2.5 Pro"),
];

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_gcloud_path")]
    pub gcloud_path: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
}

fn default_gcloud_path() -> String {
    "/app/.runtime/data/cpr-vertex-ai-adapter/google-cloud-sdk/bin/gcloud".to_owned()
}

fn default_data_dir() -> String {
    "/app/.runtime/data/cpr-vertex-ai-adapter".to_owned()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gcloud_path: default_gcloud_path(),
            data_dir: default_data_dir(),
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), PluginFault> {
        let data_dir = PathBuf::from(&self.data_dir);
        if self.gcloud_path.is_empty()
            || self.gcloud_path.len() > 512
            || self.gcloud_path.chars().any(char::is_control)
            || !data_dir.is_absolute()
            || self.data_dir.len() > 1024
            || self.data_dir.chars().any(char::is_control)
            || data_dir.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "Invalid Vertex AI adapter runtime configuration.",
            ));
        }
        Ok(())
    }

    pub fn data_dir(&self) -> PathBuf {
        PathBuf::from(&self.data_dir)
    }
    pub fn home_dir(&self) -> PathBuf {
        self.data_dir().join("home")
    }
    pub fn gcloud_config_dir(&self) -> PathBuf {
        self.data_dir().join("gcloud")
    }
    pub fn pending_root(&self) -> PathBuf {
        self.data_dir().join("pending")
    }
    pub fn pending_dir(&self, session_id: &str) -> PathBuf {
        self.pending_root().join(session_id)
    }
    pub fn adc_file(&self) -> PathBuf {
        self.gcloud_config_dir()
            .join("application_default_credentials.json")
    }
    pub fn project_file(&self) -> PathBuf {
        self.data_dir().join("project.json")
    }

    pub fn project_base_url() -> &'static str {
        "https://aiplatform.googleapis.com/v1/"
    }

    pub fn project_path(
        project_id: &str,
        location: &str,
        model: &str,
        streaming: bool,
    ) -> Result<String, PluginFault> {
        if !valid_project_id(project_id)
            || !valid_location(location)
            || Self::upstream_model(&Self::public_model(model)).is_none()
        {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "Invalid Vertex project, location, or model.",
            ));
        }
        let action = if streaming {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        Ok(format!(
            "projects/{project_id}/locations/{location}/publishers/google/models/{model}:{action}"
        ))
    }

    pub fn public_model(model: &str) -> String {
        format!("{MODEL_PREFIX}{model}")
    }

    pub fn upstream_model(public_model: &str) -> Option<&str> {
        let model = public_model.strip_prefix(MODEL_PREFIX)?;
        GEMINI_MODELS
            .iter()
            .any(|(known, _)| *known == model)
            .then_some(model)
    }

    pub fn adapter_registration(account_id: &str) -> UpstreamAdapterRegistration {
        UpstreamAdapterRegistration {
            adapters: vec![UpstreamAdapterDeclaration {
                id: ADAPTER_ID.to_owned(),
                provider: BuiltinProvider::OpenAi,
                base_url: Self::project_base_url().to_owned(),
                paths: vec![],
                authentication_kinds: vec!["api_key".to_owned()],
                transport: UpstreamTransport::HttpSse,
                protocol: "openai".to_owned(),
                // CPR 3.38.2 requires a non-empty static account scope when
                // a contributed model references this adapter.
                models: vec![],
                account_ids: vec![account_id.to_owned()],
            }],
        }
    }

    pub fn catalog_registration() -> ModelCatalogRegistrationV2 {
        ModelCatalogRegistrationV2 {
            aliases: vec![],
            models: GEMINI_MODELS.iter().map(|(model, display_name)| AdaptedModel {
                id: Self::public_model(model),
                adapter_id: ADAPTER_ID.to_owned(),
                features: vec![],
                max_output_tokens: Some(65_536),
                presentation: ModelPresentation {
                    display_name: Some((*display_name).to_owned()),
                    description: Some("Gemini text generation via Google Vertex AI. Availability depends on the selected project and location.".to_owned()),
                    context_window_tokens: Some(1_048_576),
                    max_context_window_tokens: Some(1_048_576),
                    ..ModelPresentation::default()
                },
            }).collect(),
        }
    }
}

pub fn valid_project_id(input: &str) -> bool {
    (6..=30).contains(&input.len())
        && input.starts_with(|c: char| c.is_ascii_lowercase())
        && input.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && input
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

pub fn valid_location(input: &str) -> bool {
    (2..=64).contains(&input.len())
        && input
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && !input.starts_with('-')
        && !input.ends_with('-')
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectSelection {
    pub project_id: String,
    pub location: String,
    pub email: String,
    pub account_id: String,
    pub billing_enabled: Option<bool>,
    pub billing_account: Option<String>,
    pub selected_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_path_is_validated_and_uses_vertex_publisher_path() {
        assert_eq!(
            Config::project_path("example-project-123", "global", "gemini-2.5-flash", false)
                .unwrap(),
            "projects/example-project-123/locations/global/publishers/google/models/gemini-2.5-flash:generateContent"
        );
        assert!(
            Config::project_path("../evil-project", "global", "gemini-2.5-flash", false).is_err()
        );
        assert!(
            Config::project_path(
                "example-project-123",
                "global.example.com",
                "gemini-2.5-flash",
                false
            )
            .is_err()
        );
        assert!(
            Config::project_path("example-project-123", "global", "gemini-unknown", false).is_err()
        );
    }

    #[test]
    fn registration_declares_the_managed_account_and_supported_models() {
        let adapter = Config::adapter_registration("acct_vertex_test")
            .adapters
            .pop()
            .unwrap();
        assert!(adapter.models.is_empty());
        assert_eq!(adapter.account_ids, ["acct_vertex_test"]);
        assert_eq!(adapter.authentication_kinds, ["api_key"]);
        assert_eq!(adapter.transport, UpstreamTransport::HttpSse);
        let catalog = Config::catalog_registration();
        assert_eq!(catalog.models.len(), GEMINI_MODELS.len());
        assert!(
            catalog
                .models
                .iter()
                .all(|m| m.id.starts_with(MODEL_PREFIX))
        );
    }

    #[test]
    fn data_directory_rejects_relative_and_parent_paths() {
        let mut config = Config::default();
        assert!(config.validate().is_ok());
        config.data_dir = "relative/path".into();
        assert!(config.validate().is_err());
        config.data_dir = "/app/data/../secret".into();
        assert!(config.validate().is_err());
    }
}
