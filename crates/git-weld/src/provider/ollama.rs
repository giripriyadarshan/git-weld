//! System One models served by Ollama (`kev`, `laya`, `clef`, and others)
//! through its `POST /v1/systemone` endpoint.

use super::{
    Env, Evaluation, ProviderError, SystemOneProvider,
    http::{SystemOneClient, bearer_header, config, systemone_endpoint, trim_model},
};
use crate::state::SemanticState;
use async_trait::async_trait;
use reqwest::Url;
use std::{fmt, time::Duration};

pub struct OllamaProvider {
    http: SystemOneClient,
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
        let http = SystemOneClient::new(endpoint, model, authorization, Self::API_KEY_ENV)?;

        Ok(Self { http })
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
        self.http = self.http.with_retries(max_retries, initial_backoff);
        self
    }
}

impl fmt::Debug for OllamaProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.http.fmt_debug("OllamaProvider", formatter)
    }
}

#[async_trait]
impl SystemOneProvider for OllamaProvider {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn cache_key(&self) -> Option<String> {
        Some(self.http.cache_key(Self::NAME))
    }

    async fn evaluate(&self, state: &SemanticState) -> Result<Evaluation, ProviderError> {
        self.http.evaluate(state).await
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
    use super::{super::test_env, *};

    #[test]
    fn reads_ollama_settings_and_accepts_bare_hosts() {
        let defaults =
            OllamaProvider::from_env(&test_env(&[("OLLAMA_MODEL", " kev:0.8b-bf16\n")])).unwrap();
        assert_eq!(
            defaults.http.endpoint(),
            "http://127.0.0.1:11434/v1/systemone"
        );
        assert_eq!(defaults.http.model(), "kev:0.8b-bf16");
        assert_eq!(
            defaults.cache_key().as_deref(),
            Some("ollama:kev:0.8b-bf16:systemone-choice-v1")
        );

        let bare = OllamaProvider::from_env(&test_env(&[
            ("OLLAMA_MODEL", "laya:latest"),
            ("OLLAMA_HOST", "localhost"),
        ]))
        .unwrap();
        assert_eq!(bare.http.endpoint(), "http://localhost:11434/v1/systemone");
        assert_eq!(bare.http.model(), "laya:latest");

        let bare_port = OllamaProvider::from_env(&test_env(&[
            ("OLLAMA_MODEL", "clef-flash:latest"),
            ("OLLAMA_HOST", "127.0.0.1:11435"),
        ]))
        .unwrap();
        assert_eq!(
            bare_port.http.endpoint(),
            "http://127.0.0.1:11435/v1/systemone"
        );

        let v1_suffix = OllamaProvider::from_env(&test_env(&[
            ("OLLAMA_MODEL", "kev:0.8b-bf16"),
            ("OLLAMA_HOST", "http://127.0.0.1:11434/v1"),
        ]))
        .unwrap();
        assert_eq!(
            v1_suffix.http.endpoint(),
            "http://127.0.0.1:11434/v1/systemone"
        );

        let remote = OllamaProvider::from_env(&test_env(&[
            ("OLLAMA_MODEL", "kev:0.8b-bf16"),
            ("OLLAMA_HOST", "https://ollama.example.com/api"),
            ("OLLAMA_API_KEY", "ollama-secret"),
        ]))
        .unwrap();
        assert_eq!(
            remote.http.endpoint(),
            "https://ollama.example.com/api/v1/systemone"
        );
        let debug = format!("{remote:?}");
        assert!(debug.contains("kev:0.8b-bf16"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(!debug.contains("ollama-secret"), "{debug}");

        assert_eq!(
            OllamaProvider::from_env(&test_env(&[]))
                .unwrap_err()
                .to_string(),
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
