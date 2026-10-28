//! System One models served by Ollama (`kev`, `laya`, `clef`, and others)
//! through its `POST /v1/systemone` endpoint.

use super::{
    Decision, Env, ProviderError, SystemOneProvider,
    jev::{
        PROMPT_REVISION, Retry, bearer_header, config, decisions, encode_request, http_client,
        send_request, systemone_endpoint, trim_model, would_trim_request,
    },
};
use crate::state::SemanticState;
use async_trait::async_trait;
use reqwest::{Client, Url, header::HeaderValue};
use std::{fmt, time::Duration};

pub struct OllamaProvider {
    client: Client,
    endpoint: Url,
    authorization: Option<HeaderValue>,
    model: String,
    retry: Retry,
}

impl OllamaProvider {
    pub const NAME: &str = "ollama";
    pub const MODEL_ENV: &str = "OLLAMA_MODEL";
    pub const HOST_ENV: &str = "OLLAMA_HOST";
    pub const API_KEY_ENV: &str = "OLLAMA_API_KEY";
    pub const DEFAULT_HOST: &str = "http://127.0.0.1:11434";

    /// Builds a client for an Ollama `/v1/systemone` endpoint. `host` may be a
    /// full `http(s)://` URL or a bare `host` / `host:port` address; plain HTTP
    /// is accepted only for loopback hosts.
    pub fn new(host: &str, model: &str, api_key: Option<&str>) -> Result<Self, ProviderError> {
        let model = trim_model(Self::MODEL_ENV, model)?;
        let authorization = api_key
            .map(|key| bearer_header(Self::API_KEY_ENV, key))
            .transpose()?;
        let endpoint = ollama_endpoint(host)?;
        let client = http_client(&endpoint)?;

        Ok(Self {
            client,
            endpoint,
            authorization,
            model,
            retry: Retry::default(),
        })
    }

    /// Reads `OLLAMA_MODEL` (required), `OLLAMA_HOST` (default
    /// `http://127.0.0.1:11434`), and optional `OLLAMA_API_KEY`.
    pub fn from_env(env: Env) -> Result<Self, ProviderError> {
        let model = env(Self::MODEL_ENV)
            .ok_or_else(|| config(format!("{} is not set", Self::MODEL_ENV)))?;
        let host = env(Self::HOST_ENV)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| Self::DEFAULT_HOST.to_owned());
        let api_key = env(Self::API_KEY_ENV).filter(|value| !value.trim().is_empty());
        Self::new(&host, &model, api_key.as_deref())
    }

    pub fn with_retries(mut self, max_retries: u32, initial_backoff: Duration) -> Self {
        self.retry.max_retries = max_retries;
        self.retry.initial_backoff = initial_backoff;
        self
    }

    pub fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

impl fmt::Debug for OllamaProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OllamaProvider")
            .field("endpoint", &self.endpoint.as_str())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SystemOneProvider for OllamaProvider {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn cache_key(&self) -> Option<String> {
        Some(format!("{}:{}:{PROMPT_REVISION}", Self::NAME, self.model))
    }

    fn would_trim(&self, state: &SemanticState) -> bool {
        would_trim_request(&self.model, state)
    }

    async fn evaluate(&self, state: &SemanticState) -> Result<Vec<Decision>, ProviderError> {
        let (body, _) = encode_request(&self.model, state)?;
        let response = send_request(
            &self.client,
            &self.endpoint,
            self.authorization.as_ref(),
            Self::API_KEY_ENV,
            self.retry,
            body,
        )
        .await?;
        decisions(&response, state.chunks.len())
    }
}

fn ollama_endpoint(host: &str) -> Result<Url, ProviderError> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err(config(format!("{} is empty", OllamaProvider::HOST_ENV)));
    }
    if trimmed.contains("://") {
        return systemone_endpoint(OllamaProvider::HOST_ENV, host, trimmed);
    }

    let candidate = format!("http://{trimmed}");
    let mut url = systemone_endpoint(OllamaProvider::HOST_ENV, host, &candidate)?;
    if url.port().is_none() {
        let _ = url.set_port(Some(11434));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn reads_ollama_settings_and_accepts_bare_hosts() {
        let env = |pairs: &[(&str, &str)]| {
            let values: HashMap<String, String> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            move |key: &str| values.get(key).cloned()
        };

        let defaults =
            OllamaProvider::from_env(&env(&[("OLLAMA_MODEL", " kev:0.8b-bf16\n")])).unwrap();
        assert_eq!(defaults.endpoint(), "http://127.0.0.1:11434/v1/systemone");
        assert_eq!(defaults.model(), "kev:0.8b-bf16");
        assert_eq!(
            defaults.cache_key().as_deref(),
            Some("ollama:kev:0.8b-bf16:systemone-choice-v1")
        );

        let bare = OllamaProvider::from_env(&env(&[
            ("OLLAMA_MODEL", "laya:latest"),
            ("OLLAMA_HOST", "localhost"),
        ]))
        .unwrap();
        assert_eq!(bare.endpoint(), "http://localhost:11434/v1/systemone");
        assert_eq!(bare.model(), "laya:latest");

        let bare_port = OllamaProvider::from_env(&env(&[
            ("OLLAMA_MODEL", "clef-flash:latest"),
            ("OLLAMA_HOST", "127.0.0.1:11435"),
        ]))
        .unwrap();
        assert_eq!(bare_port.endpoint(), "http://127.0.0.1:11435/v1/systemone");

        let remote = OllamaProvider::from_env(&env(&[
            ("OLLAMA_MODEL", "kev:0.8b-bf16"),
            ("OLLAMA_HOST", "https://ollama.example.com/api"),
            ("OLLAMA_API_KEY", "ollama-secret"),
        ]))
        .unwrap();
        assert_eq!(
            remote.endpoint(),
            "https://ollama.example.com/api/v1/systemone"
        );
        let debug = format!("{remote:?}");
        assert!(debug.contains("kev:0.8b-bf16"), "{debug}");
        assert!(!debug.contains("ollama-secret"), "{debug}");

        assert_eq!(
            OllamaProvider::from_env(&env(&[])).unwrap_err().to_string(),
            "OLLAMA_MODEL is not set"
        );
    }

    #[test]
    fn rejects_invalid_or_insecure_ollama_settings() {
        for (host, model, api_key, message) in [
            (
                "http://127.0.0.1:11434",
                "  ",
                None,
                "OLLAMA_MODEL is empty",
            ),
            ("  ", "kev:0.8b-bf16", None, "OLLAMA_HOST is empty"),
            (
                "http://ollama.example.com:11434",
                "kev:0.8b-bf16",
                None,
                "OLLAMA_HOST `http://ollama.example.com:11434` must use https \
                 (plain http is allowed only for localhost)",
            ),
            (
                "ollama.example.com:11434",
                "kev:0.8b-bf16",
                None,
                "OLLAMA_HOST `ollama.example.com:11434` must use https \
                 (plain http is allowed only for localhost)",
            ),
            (
                "http://127.0.0.1:11434",
                "kev:0.8b-bf16",
                Some("bad key"),
                "OLLAMA_API_KEY must be printable ASCII without spaces",
            ),
        ] {
            let error = OllamaProvider::new(host, model, api_key).unwrap_err();
            assert_eq!(error, ProviderError::Config(message.to_owned()), "{host}");
        }
    }
}
