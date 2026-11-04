//! CPR 3.38+ 兼容配置、Gemini 4 Argon 模型目录和安全路径校验。

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
pub const UPSTREAM_TRANSPORT: UpstreamTransport = UpstreamTransport::HttpSse;
pub const GEMINI_MODEL_ID: &str = "gemini-4-argon";
pub const GEMINI_MODEL_NAME: &str = "Gemini 4 Argon";
pub const GEMINI4_LOCATIONS: &[&str] = &["global", "us", "eu"];

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
        (model == GEMINI_MODEL_ID).then_some(GEMINI_MODEL_ID)
    }

    pub fn adapter_registration(account_id: &str) -> UpstreamAdapterRegistration {
        UpstreamAdapterRegistration {
            adapters: vec![UpstreamAdapterDeclaration {
                id: ADAPTER_ID.to_owned(),
                provider: BuiltinProvider::OpenAi,
                base_url: Self::project_base_url().to_owned(),
                paths: vec![],
                authentication_kinds: vec!["api_key".to_owned()],
                transport: UPSTREAM_TRANSPORT,
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
            models: vec![AdaptedModel {
                id: Self::public_model(GEMINI_MODEL_ID),
                adapter_id: ADAPTER_ID.to_owned(),
                features: vec![],
                max_output_tokens: Some(32_768),
                presentation: ModelPresentation {
                    display_name: Some(GEMINI_MODEL_NAME.to_owned()),
                    description: Some("Gemini 4 Argon via Vertex AI. The input and output share a 1,048,576-token budget; this adapter exposes text-only Responses.".to_owned()),
                    context_window_tokens: Some(1_048_576),
                    max_context_window_tokens: Some(1_048_576),
                    ..ModelPresentation::default()
                },
            }],
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
    GEMINI4_LOCATIONS.contains(&input)
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
    fn project_path_is_validated_and_uses_the_gemini_4_vertex_path() {
        assert_eq!(
            Config::project_path("example-project-123", "global", GEMINI_MODEL_ID, false).unwrap(),
            "projects/example-project-123/locations/global/publishers/google/models/gemini-4-argon:generateContent"
        );
        assert!(Config::project_path("../evil-project", "global", GEMINI_MODEL_ID, false).is_err());
        assert!(
            Config::project_path(
                "example-project-123",
                "global.example.com",
                GEMINI_MODEL_ID,
                false
            )
            .is_err()
        );
        assert!(
            Config::project_path("example-project-123", "global", "gemini-3.8-flash", false)
                .is_err()
        );
    }

    #[test]
    fn registration_declares_one_gemini_4_model_and_the_managed_account() {
        let adapter = Config::adapter_registration("acct_vertex_test")
            .adapters
            .pop()
            .unwrap();
        assert!(adapter.models.is_empty());
        assert_eq!(adapter.account_ids, ["acct_vertex_test"]);
        assert_eq!(adapter.authentication_kinds, ["api_key"]);
        assert_eq!(adapter.transport, UPSTREAM_TRANSPORT);

        let catalog = Config::catalog_registration();
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].id, "vertex/google/gemini-4-argon");
        assert_eq!(catalog.models[0].max_output_tokens, Some(32_768));
        assert_eq!(
            catalog.models[0].presentation.display_name.as_deref(),
            Some("Gemini 4 Argon")
        );
    }

    #[test]
    fn only_gemini_4_argon_is_routable_and_only_supported_locations_are_accepted() {
        assert_eq!(
            Config::upstream_model("vertex/google/gemini-4-argon"),
            Some("gemini-4-argon")
        );
        for retired in [
            "vertex/google/gemini-3.8-flash",
            "vertex/google/gemini-3.5-flash",
            "vertex/google/gemini-3.5-flash-lite",
            "vertex/google/gemini-3.1-pro-preview",
        ] {
            assert!(Config::upstream_model(retired).is_none());
        }
        for location in GEMINI4_LOCATIONS {
            assert!(valid_location(location));
        }
        assert!(!valid_location("us-central1"));
        assert!(!valid_location("us-east5"));
        assert!(!valid_location("europe-west4"));
        assert!(!valid_location("europe-west2"));
        assert!(!valid_location("asia-southeast1"));
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
