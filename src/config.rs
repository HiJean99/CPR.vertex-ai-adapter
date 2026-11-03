//! 插件设置校验与静态上游注册，不读取凭据或宿主数据库

use std::collections::BTreeSet;

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
use serde::Deserialize;

pub const ADAPTER_ID: &str = "mingli42.vertex-ai-adapter.vertex-gemini";
pub const MODEL_PREFIX: &str = "vertex/google/";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub project_id: String,
    #[serde(default)]
    pub quota_project_id: String,
    #[serde(default = "default_location")]
    pub location: String,
    pub account_id: String,
    pub models: Vec<String>,
}

fn default_location() -> String {
    "global".to_owned()
}

impl Config {
    pub fn validate(&self) -> Result<(), PluginFault> {
        let valid_project = |input: &str| {
            (6..=63).contains(&input.len())
                && input.starts_with(|c: char| c.is_ascii_lowercase())
                && input.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
                && input
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        };
        let valid_location = |input: &str| {
            (2..=64).contains(&input.len())
                && input
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && !input.starts_with('-')
                && !input.ends_with('-')
        };
        let valid_model = |input: &str| {
            (3..=128).contains(&input.len())
                && input.bytes().all(|c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'.')
                })
                && !input.starts_with('.')
                && !input.ends_with('.')
        };
        if !valid_project(&self.project_id)
            || (!self.quota_project_id.is_empty() && !valid_project(&self.quota_project_id))
            || !valid_location(&self.location)
            || self.account_id.is_empty()
            || self.account_id.len() > 128
            || !self
                .account_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
            || self.models.is_empty()
            || self.models.len() > 32
            || self.models.iter().any(|model| !valid_model(model))
            || self.models.iter().collect::<BTreeSet<_>>().len() != self.models.len()
        {
            return Err(PluginFault::new(
                ErrorCode::InvalidInput,
                "Invalid Vertex AI adapter configuration.",
            ));
        }
        Ok(())
    }

    pub fn public_model(&self, model: &str) -> String {
        format!("{MODEL_PREFIX}{model}")
    }

    pub fn upstream_model<'a>(&self, public_model: &'a str) -> Option<&'a str> {
        let model = public_model.strip_prefix(MODEL_PREFIX)?;
        self.models
            .iter()
            .any(|known| known == model)
            .then_some(model)
    }

    pub fn quota_project(&self) -> &str {
        if self.quota_project_id.is_empty() {
            &self.project_id
        } else {
            &self.quota_project_id
        }
    }

    pub fn base_url(&self) -> String {
        let host = if self.location == "global" {
            "aiplatform.googleapis.com".to_owned()
        } else {
            format!("{}-aiplatform.googleapis.com", self.location)
        };
        format!(
            "https://{host}/v1/projects/{}/locations/{}/publishers/google/models/",
            self.project_id, self.location
        )
    }

    pub fn adapter_registration(&self) -> UpstreamAdapterRegistration {
        UpstreamAdapterRegistration {
            adapters: vec![UpstreamAdapterDeclaration {
                id: ADAPTER_ID.to_owned(),
                provider: BuiltinProvider::OpenAi,
                base_url: self.base_url(),
                paths: vec![],
                authentication_kinds: vec!["api_key".to_owned()],
                transport: UpstreamTransport::HttpJson,
                protocol: "openai".to_owned(),
                models: self
                    .models
                    .iter()
                    .map(|model| self.public_model(model))
                    .collect(),
                account_ids: vec![self.account_id.clone()],
            }],
        }
    }

    pub fn catalog_registration(&self) -> ModelCatalogRegistrationV2 {
        ModelCatalogRegistrationV2 {
            aliases: vec![],
            models: self
                .models
                .iter()
                .map(|model| AdaptedModel {
                    id: self.public_model(model),
                    adapter_id: ADAPTER_ID.to_owned(),
                    features: vec![],
                    max_output_tokens: None,
                    presentation: ModelPresentation {
                        display_name: Some(format!("Vertex AI Gemini · {model}")),
                        description: Some("Text-only Gemini via Google Vertex AI".to_owned()),
                        ..ModelPresentation::default()
                    },
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Config {
        Config {
            project_id: "example-project-123".into(),
            quota_project_id: String::new(),
            location: "global".into(),
            account_id: "acct_example".into(),
            models: vec!["gemini-2.5-flash".into()],
        }
    }

    #[test]
    fn register_explicit_account_and_model_only() {
        let config = sample();
        config.validate().unwrap();
        let adapter = config.adapter_registration().adapters.pop().unwrap();
        assert_eq!(adapter.account_ids, ["acct_example"]);
        assert_eq!(adapter.models, ["vertex/google/gemini-2.5-flash"]);
        assert_eq!(
            adapter.base_url,
            "https://aiplatform.googleapis.com/v1/projects/example-project-123/locations/global/publishers/google/models/"
        );
        assert_eq!(
            config.upstream_model("vertex/google/gemini-2.5-flash"),
            Some("gemini-2.5-flash")
        );
        assert_eq!(config.upstream_model("gpt-6.1-terra"), None);
    }

    #[test]
    fn reject_url_segments_and_duplicate_models() {
        let mut config = sample();
        config.location = "global.example.com".into();
        assert!(config.validate().is_err());
        config.location = "us-central1".into();
        config.models.push("gemini-2.5-flash".into());
        assert!(config.validate().is_err());
        config.models.pop();
        config.models[0] = "../danger".into();
        assert!(config.validate().is_err());
    }
}
