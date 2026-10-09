//! Jev, TypeSafe AI's System One model (<https://docs.typesafe.ai>).
//!
//! All conflicting items of a file go out in one `POST /v1/systemone`
//! request: the semantic state as JSON plus one Choice question per item.
//! Jev answers each question with one of our options, a probability for every
//! option, and a calibrated confidence, so no free text needs parsing.

use super::{
    Confidence, Decision, Env, MergeAction, ProviderError, SystemOneProvider, error_chain,
};
use crate::state::SemanticState;
use async_trait::async_trait;
use reqwest::{
    Client, StatusCode, Url,
    header::{self, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
use std::{collections::BTreeMap, fmt, net::IpAddr, time::Duration};

// The question asked about every conflicting item. `item` is the item's path
// in the state, such as `conflicts[0]`; see `SemanticState` for its fields.
fn instructions(item: &str) -> String {
    format!(
        "`{item}` is one code item that two branches of a merge changed in different ways. \
         Which version should the merged file contain?"
    )
}

// Jev 1.13 leans toward the first option, so the safe one comes first.
const OPTIONS: [MergeAction; 3] = [
    MergeAction::Manual,
    MergeAction::TakeOurs,
    MergeAction::TakeTheirs,
];

fn criterion(option: MergeAction, item: &str) -> String {
    match option {
        MergeAction::Manual => format!(
            "Neither version alone. The merged item needs changes from both `{item}.ours` and \
             `{item}.theirs`, the two changes contradict each other, or the right result is \
             unclear."
        ),
        MergeAction::TakeOurs => format!(
            "`{item}.ours` exactly as it is (null means the item is removed). It already \
             includes the change made in `{item}.theirs` or an equivalent one, so nothing from \
             `{item}.theirs` is lost."
        ),
        MergeAction::TakeTheirs => format!(
            "`{item}.theirs` exactly as it is (null means the item is removed). It already \
             includes the change made in `{item}.ours` or an equivalent one, so nothing from \
             `{item}.ours` is lost."
        ),
    }
}

// Roughly the 32k tokens Jev 1.13 accepts for the state and one question.
const MAX_REQUEST_BYTES: usize = 128 * 1024;
const MAX_ERROR_CHARS: usize = 200;
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

pub struct JevProvider {
    client: Client,
    endpoint: Url,
    authorization: HeaderValue,
    model: String,
    retry: Retry,
}

#[derive(Clone, Copy, Debug)]
struct Retry {
    max_retries: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            max_retries: 2,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(2),
        }
    }
}

impl Retry {
    fn backoff(self, attempt: u32) -> Duration {
        self.initial_backoff
            .saturating_mul(2_u32.saturating_pow(attempt))
            .min(self.max_backoff)
    }
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
        let api_key = api_key.trim();
        if api_key.is_empty() {
            return Err(config(format!("{} is empty", Self::API_KEY_ENV)));
        }
        if !api_key.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(config(format!(
                "{} must be printable ASCII without spaces",
                Self::API_KEY_ENV
            )));
        }
        let mut authorization = HeaderValue::try_from(format!("Bearer {api_key}"))
            .map_err(|_| config(format!("{} is not a valid header value", Self::API_KEY_ENV)))?;
        authorization.set_sensitive(true);

        let model = model.trim();
        if model.is_empty() {
            return Err(config(format!("{} is empty", Self::MODEL_ENV)));
        }
        let endpoint = endpoint(base_url)?;
        let mut client = Client::builder().user_agent(USER_AGENT);
        if is_loopback(&endpoint) {
            client = client.no_proxy();
        }
        let client = client.build().map_err(|error| {
            config(format!(
                "cannot create an HTTP client: {}",
                error_chain(&error)
            ))
        })?;

        Ok(Self {
            client,
            endpoint,
            authorization,
            model: model.to_owned(),
            retry: Retry::default(),
        })
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

    fn request_body(&self, state: &SemanticState) -> Result<Vec<u8>, ProviderError> {
        let request = Request {
            model: &self.model,
            state,
            questions: Questions(state.conflicts.len()),
        };
        let body = serde_json::to_vec(&request).map_err(|error| {
            ProviderError::Transport(format!("cannot encode the request: {error}"))
        })?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(ProviderError::TooLarge {
                bytes: body.len(),
                limit: MAX_REQUEST_BYTES,
            });
        }
        Ok(body)
    }

    async fn send(&self, body: Vec<u8>) -> Result<Vec<u8>, ProviderError> {
        let mut attempt = 0;
        loop {
            let sent = self
                .client
                .post(self.endpoint.clone())
                .header(header::AUTHORIZATION, self.authorization.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT, "application/json")
                .body(body.clone())
                .send()
                .await;
            let (error, requested_wait) = match sent {
                Ok(response) if response.status().is_success() => {
                    return response
                        .bytes()
                        .await
                        .map(|bytes| bytes.to_vec())
                        .map_err(|error| ProviderError::Transport(error_chain(&error)));
                }
                Ok(response) => {
                    let status = response.status();
                    let wait = retry_after(response.headers());
                    let mut message = summarize(&response.text().await.unwrap_or_default());
                    if status == StatusCode::UNAUTHORIZED {
                        message = format!("{message} (check {})", Self::API_KEY_ENV)
                            .trim_start()
                            .to_owned();
                    }
                    let error = ProviderError::Http {
                        status: status.as_u16(),
                        message,
                    };
                    if !is_retryable(status) {
                        return Err(error);
                    }
                    (error, wait)
                }
                Err(error) if error.is_connect() => {
                    (ProviderError::Transport(error_chain(&error)), None)
                }
                Err(error) => return Err(ProviderError::Transport(error_chain(&error))),
            };

            if attempt >= self.retry.max_retries {
                return Err(error);
            }
            let wait = match requested_wait {
                Some(wait) if wait > self.retry.max_backoff => return Err(error),
                Some(wait) => wait,
                None => self.retry.backoff(attempt),
            };
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }
}

// The API key is left out.
impl fmt::Debug for JevProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JevProvider")
            .field("endpoint", &self.endpoint.as_str())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SystemOneProvider for JevProvider {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn evaluate(&self, state: &SemanticState) -> Result<Vec<Decision>, ProviderError> {
        let body = self.request_body(state)?;
        let response = self.send(body).await?;
        decisions(&response, state.conflicts.len())
    }
}

fn config(message: String) -> ProviderError {
    ProviderError::Config(message)
}

fn invalid(message: String) -> ProviderError {
    ProviderError::InvalidResponse(message)
}

fn endpoint(base_url: &str) -> Result<Url, ProviderError> {
    let name = JevProvider::BASE_URL_ENV;
    let mut url = Url::parse(base_url.trim())
        .map_err(|error| config(format!("{name} is not a valid URL: {error}")))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(config(format!("{name} must not contain credentials")));
    }
    if url.cannot_be_a_base() || url.query().is_some() || url.fragment().is_some() {
        return Err(config(format!(
            "{name} `{base_url}` must be a plain http(s) address"
        )));
    }
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(&url) => {}
        _ => {
            return Err(config(format!(
                "{name} `{base_url}` must use https (plain http is allowed only for localhost)"
            )));
        }
    }
    let path = format!("{}/v1/systemone", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    })
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

// TypeSafe sends `retry-after-ms`; `retry-after` is the standard header.
// HTTP dates are ignored in favor of the normal backoff.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let number = |name: &str| -> Option<f64> {
        let value = headers
            .get(name)?
            .to_str()
            .ok()?
            .trim()
            .parse::<f64>()
            .ok()?;
        (value >= 0.0).then_some(value)
    };
    number("retry-after-ms")
        .and_then(|milliseconds| Duration::try_from_secs_f64(milliseconds / 1000.0).ok())
        .or_else(|| {
            number("retry-after").and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
        })
}

fn summarize(body: &str) -> String {
    let text = body.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(MAX_ERROR_CHARS) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text,
    }
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    state: &'a SemanticState,
    questions: Questions,
}

fn question_id(index: usize) -> String {
    format!("conflict_{index}")
}

// One Choice question per conflict, keyed `conflict_0`, `conflict_1`, ...
struct Questions(usize);

impl Serialize for Questions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut questions = serializer.serialize_map(Some(self.0))?;
        for index in 0..self.0 {
            let item = format!("conflicts[{index}]");
            questions.serialize_entry(
                &question_id(index),
                &Question {
                    kind: "choice",
                    instructions: instructions(&item),
                    criteria: Criteria(item),
                },
            )?;
        }
        questions.end()
    }
}

#[derive(Serialize)]
struct Question {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: String,
    criteria: Criteria,
}

// Serialized as a map in `OPTIONS` order.
struct Criteria(String);

impl Serialize for Criteria {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut criteria = serializer.serialize_map(Some(OPTIONS.len()))?;
        for option in OPTIONS {
            criteria.serialize_entry(option.as_str(), &criterion(option, &self.0))?;
        }
        criteria.end()
    }
}

#[derive(Deserialize)]
struct Response {
    model: String,
    answers: BTreeMap<String, Answer>,
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "type")]
    kind: String,
    choice: Option<String>,
    confidence: Option<f64>,
    probabilities: Option<BTreeMap<String, f64>>,
}

fn decisions(body: &[u8], count: usize) -> Result<Vec<Decision>, ProviderError> {
    let response: Response =
        serde_json::from_slice(body).map_err(|error| invalid(error.to_string()))?;
    (0..count)
        .map(|index| decision(&response, &question_id(index)))
        .collect()
}

fn decision(response: &Response, id: &str) -> Result<Decision, ProviderError> {
    let answer = response
        .answers
        .get(id)
        .ok_or_else(|| invalid(format!("answers.{id} is missing")))?;
    let missing = |field: &str| invalid(format!("answers.{id}.{field} is missing"));
    if answer.kind != "choice" {
        return Err(invalid(format!(
            "answers.{id}.type is `{}`, expected `choice`",
            answer.kind
        )));
    }

    let choice = answer.choice.as_deref().ok_or_else(|| missing("choice"))?;
    let action = choice
        .parse::<MergeAction>()
        .map_err(|error| invalid(format!("answers.{id}.choice: {error}")))?;
    let confidence = answer.confidence.ok_or_else(|| missing("confidence"))?;
    let confidence = Confidence::new(confidence)
        .map_err(|error| invalid(format!("answers.{id}.confidence: {error}")))?;

    let probabilities = answer
        .probabilities
        .as_ref()
        .ok_or_else(|| missing("probabilities"))?;
    if probabilities.len() != OPTIONS.len() {
        return Err(invalid(format!(
            "answers.{id}.probabilities must list exactly Manual, TakeOurs, and TakeTheirs"
        )));
    }
    let mut listed = Vec::with_capacity(OPTIONS.len());
    for option in OPTIONS {
        let probability = probabilities
            .get(option.as_str())
            .copied()
            .ok_or_else(|| missing(&format!("probabilities.{option}")))?;
        if !(0.0..=1.0).contains(&probability) {
            return Err(invalid(format!(
                "answers.{id}.probabilities.{option}: {probability} is not between 0 and 1"
            )));
        }
        listed.push(format!("{option} {probability}"));
    }

    let reason = format!(
        "{} chose {action} (probabilities: {})",
        response.model,
        listed.join(", ")
    );
    Ok(Decision::new(action, confidence, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ast, merge::Versions};
    use serde_json::{Value, json};
    use std::{collections::HashMap, path::Path};

    const BASE: &str = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";

    fn state() -> SemanticState {
        let versions = Versions {
            base: BASE.as_bytes().to_vec(),
            ours: BASE
                .replace("1\n", "10\n")
                .replace("2\n", "20\n")
                .into_bytes(),
            theirs: BASE
                .replace("1\n", "11\n")
                .replace("2\n", "22\n")
                .into_bytes(),
        };
        let path = Path::new("src/lib.rs");
        let analysis =
            ast::analyze(path, &versions.base, &versions.ours, &versions.theirs).unwrap();
        SemanticState::new(path, &analysis, &versions)
    }

    fn provider() -> JevProvider {
        JevProvider::new(JevProvider::DEFAULT_BASE_URL, "ts-secret", "jev-latest").unwrap()
    }

    fn answer(choice: &str, confidence: f64, probabilities: [f64; 3]) -> Value {
        json!({
            "type": "choice",
            "choice": choice,
            "confidence": confidence,
            "probabilities": {
                "TakeOurs": probabilities[1],
                "TakeTheirs": probabilities[2],
                "Manual": probabilities[0],
            },
        })
    }

    fn response(answers: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model": "jev-1.13.0",
            "answers": answers,
            "usage": {"input_tokens": 512, "output_tokens": 40},
        }))
        .unwrap()
    }

    #[test]
    fn asks_one_choice_question_per_conflict_about_the_semantic_state() {
        let state = state();

        let body = provider().request_body(&state).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(request["model"], "jev-latest");
        assert_eq!(request["state"], serde_json::to_value(&state).unwrap());
        assert_eq!(
            request["questions"],
            json!({
                "conflict_0": {
                    "type": "choice",
                    "instructions": "`conflicts[0]` is one code item that two branches of a \
                                     merge changed in different ways. Which version should the \
                                     merged file contain?",
                    "criteria": {
                        "Manual": criterion(MergeAction::Manual, "conflicts[0]"),
                        "TakeOurs": criterion(MergeAction::TakeOurs, "conflicts[0]"),
                        "TakeTheirs": criterion(MergeAction::TakeTheirs, "conflicts[0]"),
                    },
                },
                "conflict_1": {
                    "type": "choice",
                    "instructions": instructions("conflicts[1]"),
                    "criteria": {
                        "Manual": criterion(MergeAction::Manual, "conflicts[1]"),
                        "TakeOurs": criterion(MergeAction::TakeOurs, "conflicts[1]"),
                        "TakeTheirs": criterion(MergeAction::TakeTheirs, "conflicts[1]"),
                    },
                },
            })
        );
        assert_eq!(
            criterion(MergeAction::TakeOurs, "conflicts[1]"),
            "`conflicts[1].ours` exactly as it is (null means the item is removed). It already \
             includes the change made in `conflicts[1].theirs` or an equivalent one, so nothing \
             from `conflicts[1].theirs` is lost."
        );

        let text = String::from_utf8(body).unwrap();
        let position = |needle: &str| text.find(needle).unwrap();
        assert!(position("\"Manual\":") < position("\"TakeOurs\":"));
        assert!(position("\"TakeOurs\":") < position("\"TakeTheirs\":"));
    }

    #[test]
    fn refuses_requests_over_the_size_limit() {
        let mut state = state();
        state.conflicts[0].base.as_mut().unwrap().code = "x".repeat(MAX_REQUEST_BYTES);

        let error = provider().request_body(&state).unwrap_err();

        assert!(
            matches!(error, ProviderError::TooLarge { bytes, limit: MAX_REQUEST_BYTES } if bytes > MAX_REQUEST_BYTES),
            "{error:?}"
        );
    }

    #[test]
    fn turns_choice_answers_into_decisions() {
        let body = response(json!({
            "conflict_0": answer("TakeTheirs", 0.97, [0.01, 0.01, 0.98]),
            "conflict_1": answer("Manual", 0.4, [0.6, 0.3, 0.1]),
        }));

        let decisions = decisions(&body, 2).unwrap();

        assert_eq!(
            decisions,
            [
                Decision::new(
                    MergeAction::TakeTheirs,
                    Confidence::new(0.97).unwrap(),
                    "jev-1.13.0 chose TakeTheirs \
                     (probabilities: Manual 0.01, TakeOurs 0.01, TakeTheirs 0.98)"
                ),
                Decision::new(
                    MergeAction::Manual,
                    Confidence::new(0.4).unwrap(),
                    "jev-1.13.0 chose Manual \
                     (probabilities: Manual 0.6, TakeOurs 0.3, TakeTheirs 0.1)"
                ),
            ]
        );
    }

    #[test]
    fn rejects_answers_that_do_not_fit_the_questions() {
        let valid = answer("TakeOurs", 0.99, [0.0, 0.99, 0.01]);
        let with = |field: &str, value: Value| {
            let mut answer = valid.clone();
            answer[field] = value;
            response(json!({ "conflict_0": answer }))
        };
        let without = |field: &str| {
            let mut answer = valid.clone();
            answer.as_object_mut().unwrap().remove(field);
            response(json!({ "conflict_0": answer }))
        };

        for (body, message) in [
            (b"not json".to_vec(), "expected ident at line 1 column 2"),
            (
                serde_json::to_vec(&json!({"answers": {}})).unwrap(),
                "missing field `model` at line 1 column 14",
            ),
            (response(json!({})), "answers.conflict_0 is missing"),
            (
                response(json!({"conflict_0": {"type": "noul", "noul": 0.9}})),
                "answers.conflict_0.type is `noul`, expected `choice`",
            ),
            (
                with("choice", json!("TakeBoth")),
                "answers.conflict_0.choice: unknown decision `TakeBoth` \
                 (expected TakeOurs, TakeTheirs, or Manual)",
            ),
            (without("choice"), "answers.conflict_0.choice is missing"),
            (
                with("confidence", json!(1.2)),
                "answers.conflict_0.confidence: confidence 1.2 is not between 0 and 1",
            ),
            (
                without("confidence"),
                "answers.conflict_0.confidence is missing",
            ),
            (
                without("probabilities"),
                "answers.conflict_0.probabilities is missing",
            ),
            (
                with("probabilities", json!({"TakeOurs": 0.5, "TakeTheirs": 0.5})),
                "answers.conflict_0.probabilities must list exactly Manual, TakeOurs, and \
                 TakeTheirs",
            ),
            (
                with(
                    "probabilities",
                    json!({"Manual": 0.0, "TakeOurs": 1.5, "TakeTheirs": 0.0}),
                ),
                "answers.conflict_0.probabilities.TakeOurs: 1.5 is not between 0 and 1",
            ),
            (
                with(
                    "probabilities",
                    json!({"Manual": 0.0, "TakeOurs": 1.0, "Other": 0.0}),
                ),
                "answers.conflict_0.probabilities.TakeTheirs is missing",
            ),
        ] {
            let error = decisions(&body, 1).unwrap_err();
            assert_eq!(
                error,
                ProviderError::InvalidResponse(message.to_owned()),
                "{}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    #[test]
    fn reads_its_configuration_from_the_environment() {
        let env = |pairs: &[(&str, &str)]| {
            let values: HashMap<String, String> = pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect();
            move |key: &str| values.get(key).cloned()
        };

        let defaults =
            JevProvider::from_env(&env(&[("TYPESAFE_API_KEY", " ts-secret\n")])).unwrap();
        assert_eq!(defaults.endpoint(), "https://api.typesafe.ai/v1/systemone");
        assert_eq!(defaults.model(), "jev-latest");

        let custom = JevProvider::from_env(&env(&[
            ("TYPESAFE_API_KEY", "ts-secret"),
            ("TYPESAFE_BASE_URL", "https://gateway.example/typesafe/"),
            ("TYPESAFE_DEFAULT_MODEL", "jev-1.13.0"),
        ]))
        .unwrap();
        assert_eq!(
            custom.endpoint(),
            "https://gateway.example/typesafe/v1/systemone"
        );
        assert_eq!(custom.model(), "jev-1.13.0");

        let debug = format!("{custom:?}");
        assert!(debug.contains("jev-1.13.0"), "{debug}");
        assert!(!debug.contains("ts-secret"), "{debug}");

        assert_eq!(
            JevProvider::from_env(&env(&[])).unwrap_err().to_string(),
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
            assert!(provider.endpoint().ends_with("/v1/systemone"), "{local}");
        }
    }

    #[test]
    fn honors_short_retry_after_headers() {
        let headers = |pairs: &[(&'static str, &'static str)]| {
            pairs
                .iter()
                .map(|(name, value)| {
                    (
                        header::HeaderName::from_static(name),
                        HeaderValue::from_static(value),
                    )
                })
                .collect::<HeaderMap>()
        };

        assert_eq!(
            retry_after(&headers(&[
                ("retry-after-ms", "1500"),
                ("retry-after", "9")
            ])),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            retry_after(&headers(&[("retry-after", "3")])),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_after(&headers(&[(
                "retry-after",
                "Wed, 21 Oct 2026 07:28:00 GMT"
            )])),
            None
        );
        assert_eq!(retry_after(&headers(&[("retry-after", "-1")])), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "1e400")])), None);
        assert_eq!(retry_after(&headers(&[])), None);

        let retry = Retry::default();
        assert_eq!(retry.backoff(0), Duration::from_millis(250));
        assert_eq!(retry.backoff(1), Duration::from_millis(500));
        assert_eq!(retry.backoff(10), Duration::from_secs(2));
    }

    #[test]
    fn summarizes_error_bodies_on_one_line() {
        assert_eq!(
            summarize("{\n  \"detail\": \"bad\"\n}"),
            "{ \"detail\": \"bad\" }"
        );
        let long = "x".repeat(MAX_ERROR_CHARS + 1);
        assert_eq!(summarize(&long), format!("{}...", &long[..MAX_ERROR_CHARS]));
    }
}
