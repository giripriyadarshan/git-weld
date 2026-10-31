//! Jev, TypeSafe AI's System One model (<https://docs.typesafe.ai>).
//!
//! All chunks of a file go out in one `POST /v1/systemone` request: the
//! semantic state as JSON plus one Choice question per chunk, which Jev
//! evaluates in parallel against the same state.
//! Jev answers each question with one of our options, a probability for every
//! option, and a calibrated confidence, so no free text needs parsing.

use super::{
    Env, Evaluation, ProviderError, SystemOneProvider,
    http::{SystemOneClient, bearer_header, config, systemone_endpoint, trim_model},
};
use crate::state::SemanticState;
use async_trait::async_trait;
use std::{fmt, time::Duration};

pub struct JevProvider {
    http: SystemOneClient,
}

impl JevProvider {
    pub const NAME: &str = "jev";
    /// The variable names match TypeSafe's own SDKs.
    pub const API_KEY_ENV: &str = "TYPESAFE_API_KEY";
    pub const BASE_URL_ENV: &str = "TYPESAFE_BASE_URL";
    pub const MODEL_ENV: &str = "TYPESAFE_DEFAULT_MODEL";
    pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
    pub const DEFAULT_MODEL: &str = "jev-latest";

    /// `base_url` must use https; plain http is accepted only for loopback
    /// hosts, such as a local test server, so the key never travels in clear
    /// text.
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Result<Self, ProviderError> {
        let authorization = bearer_header(Self::API_KEY_ENV, api_key)?;
        let model = trim_model(Self::MODEL_ENV, model)?;
        let endpoint = systemone_endpoint(Self::BASE_URL_ENV, base_url, base_url.trim())?;
        let http = SystemOneClient::new(endpoint, model, Some(authorization), Self::API_KEY_ENV)?;

        Ok(Self { http })
    }

    /// Reads `TYPESAFE_API_KEY` (required), `TYPESAFE_BASE_URL`, and
    /// `TYPESAFE_DEFAULT_MODEL`.
    pub fn from_env(env: Env) -> Result<Self, ProviderError> {
        let api_key = env(Self::API_KEY_ENV)
            .ok_or_else(|| config(format!("{} is not set", Self::API_KEY_ENV)))?;
        let setting = |key: &str, default: &str| {
            env(key)
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| default.to_owned())
        };
        Self::new(
            &setting(Self::BASE_URL_ENV, Self::DEFAULT_BASE_URL),
            &api_key,
            &setting(Self::MODEL_ENV, Self::DEFAULT_MODEL),
        )
    }

    /// Retries rate limits (408, 429), server errors (5xx, including 529
    /// Overloaded), and refused connections up to `max_retries` times. The
    /// wait starts at `initial_backoff` and doubles, up to 2 seconds; a
    /// `Retry-After` that asks for longer ends the retries.
    pub fn with_retries(mut self, max_retries: u32, initial_backoff: Duration) -> Self {
        self.http = self.http.with_retries(max_retries, initial_backoff);
        self
    }
}

impl fmt::Debug for JevProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.http.fmt_debug("JevProvider", formatter)
    }
}

#[async_trait]
impl SystemOneProvider for JevProvider {
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

#[cfg(test)]
mod tests {
    use super::{super::test_env, *};

    #[test]
    fn reads_its_configuration_from_the_environment() {
        let defaults =
            JevProvider::from_env(&test_env(&[("TYPESAFE_API_KEY", " ts-secret\n")])).unwrap();
        assert_eq!(
            defaults.http.endpoint(),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(defaults.http.model(), "jev-latest");
        assert_eq!(
            defaults.cache_key().as_deref(),
            Some("jev:jev-latest:systemone-choice-v1")
        );

        let custom = JevProvider::from_env(&test_env(&[
            ("TYPESAFE_API_KEY", "ts-secret"),
            ("TYPESAFE_BASE_URL", "https://gateway.example/typesafe/"),
            ("TYPESAFE_DEFAULT_MODEL", "jev-1.13.0"),
        ]))
        .unwrap();
        assert_eq!(
            custom.http.endpoint(),
            "https://gateway.example/typesafe/v1/systemone"
        );
        assert_eq!(custom.http.model(), "jev-1.13.0");

        let debug = format!("{custom:?}");
        assert!(debug.contains("jev-1.13.0"), "{debug}");
        assert!(debug.contains("[REDACTED]"), "{debug}");
        assert!(!debug.contains("ts-secret"), "{debug}");

        assert_eq!(
            JevProvider::from_env(&test_env(&[]))
                .unwrap_err()
                .to_string(),
            "TYPESAFE_API_KEY is not set"
        );
    }

    #[test]
    fn rejects_unsafe_or_invalid_settings() {
        for (base_url, api_key, model, message) in [
            (
                "https://api.typesafe.ai",
                "  ",
                "jev-latest",
                "TYPESAFE_API_KEY is empty",
            ),
            (
                "https://api.typesafe.ai",
                "ts secret",
                "jev-latest",
                "TYPESAFE_API_KEY must be printable ASCII without spaces",
            ),
            (
                "https://api.typesafe.ai",
                "ts-sécret",
                "jev-latest",
                "TYPESAFE_API_KEY must be printable ASCII without spaces",
            ),
            (
                "https://api.typesafe.ai",
                "ts-secret",
                " ",
                "TYPESAFE_DEFAULT_MODEL is empty",
            ),
            (
                "api.typesafe.ai",
                "ts-secret",
                "jev-latest",
                "TYPESAFE_BASE_URL is not a valid URL: relative URL without a base",
            ),
            (
                "http://api.typesafe.ai",
                "ts-secret",
                "jev-latest",
                "TYPESAFE_BASE_URL `http://api.typesafe.ai` must use https \
                 (plain http is allowed only for localhost)",
            ),
            (
                "ftp://localhost",
                "ts-secret",
                "jev-latest",
                "TYPESAFE_BASE_URL `ftp://localhost` must use https \
                 (plain http is allowed only for localhost)",
            ),
            (
                "https://user:hunter2@api.typesafe.ai",
                "ts-secret",
                "jev-latest",
                "TYPESAFE_BASE_URL must not contain credentials",
            ),
            (
                "https://api.typesafe.ai/?key=1",
                "ts-secret",
                "jev-latest",
                "TYPESAFE_BASE_URL `https://api.typesafe.ai/?key=1` must be a plain http(s) \
                 address",
            ),
        ] {
            let error = JevProvider::new(base_url, api_key, model).unwrap_err();
            assert_eq!(
                error,
                ProviderError::Config(message.to_owned()),
                "{base_url}"
            );
        }

        for local in [
            "http://localhost:8080",
            "http://127.0.0.1:9",
            "http://[::1]:9/",
        ] {
            let provider = JevProvider::new(local, "ts-secret", "jev-latest").unwrap();
            assert!(
                provider.http.endpoint().ends_with("/v1/systemone"),
                "{local}"
            );
        }
    }
}
