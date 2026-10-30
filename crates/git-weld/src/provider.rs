//! The model-agnostic System One layer: the uniform [`Decision`] every
//! provider returns, the [`SystemOneProvider`] trait, cache-aware evaluation
//! ([`evaluate_with_cache`]), and loading a provider by the name given to
//! `--provider`.

mod http;
mod jev;
mod mock;
mod ollama;

pub use jev::JevProvider;
pub use mock::MockProvider;
pub use ollama::OllamaProvider;

use crate::{
    cache::{self, Cache},
    state::SemanticState,
};
use async_trait::async_trait;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};
use serde_json::{Value, json};
use std::{
    error::Error,
    fmt,
    panic::{self, AssertUnwindSafe},
    str::FromStr,
    time::Duration,
};

/// A decision may resolve its chunk without a person only when its confidence
/// is strictly greater than this. A confidence of exactly 0.95 does not.
pub const AUTO_RESOLVE_THRESHOLD: f64 = 0.95;

/// The provider name that turns model evaluation off.
pub const MANUAL: &str = "manual";

/// Every name that [`load`] accepts.
pub const PROVIDERS: [&str; 4] = [
    JevProvider::NAME,
    MANUAL,
    MockProvider::NAME,
    OllamaProvider::NAME,
];

/// Looks up configuration such as API keys; `std::env::var` in the driver.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What to do with one chunk that both sides changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MergeAction {
    /// Keep our version of the chunk.
    TakeOurs,
    /// Keep their version of the chunk.
    TakeTheirs,
    /// Neither version alone is right, so a person must merge the chunk.
    Manual,
}

impl MergeAction {
    pub const ALL: [Self; 3] = [Self::TakeOurs, Self::TakeTheirs, Self::Manual];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TakeOurs => "TakeOurs",
            Self::TakeTheirs => "TakeTheirs",
            Self::Manual => "Manual",
        }
    }
}

impl fmt::Display for MergeAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for MergeAction {
    type Err = UnknownMergeAction;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == text)
            .ok_or_else(|| UnknownMergeAction(text.to_owned()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownMergeAction(pub String);

impl fmt::Display for UnknownMergeAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unknown decision `{}` (expected TakeOurs, TakeTheirs, or Manual)",
            self.0
        )
    }
}

impl Error for UnknownMergeAction {}

/// A confidence between 0 and 1, inclusive.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Confidence(f64);

impl Confidence {
    pub fn new(value: f64) -> Result<Self, InvalidConfidence> {
        if (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidConfidence(value))
        }
    }

    pub const fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Confidence {
    type Error = InvalidConfidence;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Confidence> for f64 {
    fn from(confidence: Confidence) -> Self {
        confidence.0
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InvalidConfidence(pub f64);

impl fmt::Display for InvalidConfidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "confidence {} is not between 0 and 1", self.0)
    }
}

impl Error for InvalidConfidence {}

/// The uniform answer for one chunk, whatever the backend:
/// `{"decision": "TakeOurs", "confidence": 0.98, "reason": "..."}`.
///
/// Deserializing enforces [`Decision::json_schema`]: only objects are
/// accepted, unknown fields and decisions are rejected, and so is a
/// confidence outside 0 to 1.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub decision: MergeAction,
    pub confidence: Confidence,
    pub reason: String,
}

// Serde's derived impl would also accept `["TakeOurs", 0.98, "..."]`, which
// the schema does not allow, so only maps reach the derived field handling.
impl<'de> Deserialize<'de> for Decision {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Fields {
            decision: MergeAction,
            confidence: Confidence,
            reason: String,
        }

        struct ObjectOnly;

        impl<'de> Visitor<'de> for ObjectOnly {
            type Value = Decision;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a decision object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Decision, A::Error> {
                let fields = Fields::deserialize(MapAccessDeserializer::new(map))?;
                Ok(Decision::new(
                    fields.decision,
                    fields.confidence,
                    fields.reason,
                ))
            }
        }

        deserializer.deserialize_map(ObjectOnly)
    }
}

impl Decision {
    pub fn new(decision: MergeAction, confidence: Confidence, reason: impl Into<String>) -> Self {
        Self {
            decision,
            confidence,
            reason: reason.into(),
        }
    }

    /// The JSON Schema (draft 2020-12) that a serialized decision satisfies.
    /// Providers backed by generative models can use it to constrain output.
    pub fn json_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Decision",
            "description": "How to resolve one chunk of code that both sides of a merge changed.",
            "type": "object",
            "properties": {
                "decision": {
                    "description": "Keep our version, keep their version, or leave the chunk to a person.",
                    "type": "string",
                    "enum": MergeAction::ALL.map(MergeAction::as_str),
                },
                "confidence": {
                    "description": "How certain the decision is, from 0 to 1.",
                    "type": "number",
                    "minimum": 0,
                    "maximum": 1,
                },
                "reason": {
                    "description": "Why the provider made this decision.",
                    "type": "string",
                },
            },
            "required": ["decision", "confidence", "reason"],
            "additionalProperties": false,
        })
    }

    /// Whether the decision is confident enough to resolve its chunk without
    /// a person: `TakeOurs` or `TakeTheirs` above [`AUTO_RESOLVE_THRESHOLD`].
    pub fn is_auto_resolvable(&self) -> bool {
        self.decision != MergeAction::Manual && self.confidence.value() > AUTO_RESOLVE_THRESHOLD
    }
}

/// Why a provider could not decide. Every error leads to manual resolution.
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderError {
    /// The provider is not configured correctly, e.g. its API key is missing.
    Config(String),
    /// The request would be larger than the provider accepts.
    TooLarge { bytes: usize, limit: usize },
    /// The request could not be sent or the answer could not be read.
    Transport(String),
    /// The service answered with an HTTP error status.
    Http { status: u16, message: String },
    /// The answer does not have the expected shape or values.
    InvalidResponse(String),
    /// No answer arrived in time.
    Timeout(Duration),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => formatter.write_str(message),
            Self::TooLarge { bytes, limit } => write!(
                formatter,
                "the request would be {bytes} bytes, over the {limit}-byte limit"
            ),
            Self::Transport(message) => write!(formatter, "request failed: {message}"),
            Self::Http { status, message } if message.is_empty() => {
                write!(formatter, "HTTP {status}")
            }
            Self::Http { status, message } => write!(formatter, "HTTP {status}: {message}"),
            Self::InvalidResponse(message) => write!(formatter, "invalid response: {message}"),
            Self::Timeout(limit) => write!(formatter, "no answer within {limit:?}"),
        }
    }
}

impl Error for ProviderError {}

/// The decisions for every chunk of a [`SemanticState`], plus whether hoisted
/// signatures had to be trimmed to fit the provider's request size limit.
#[derive(Clone, Debug, PartialEq)]
pub struct Evaluation {
    pub decisions: Vec<Decision>,
    pub trimmed: bool,
}

impl Evaluation {
    pub const fn new(decisions: Vec<Decision>, trimmed: bool) -> Self {
        Self { decisions, trimmed }
    }

    pub const fn untrimmed(decisions: Vec<Decision>) -> Self {
        Self {
            decisions,
            trimmed: false,
        }
    }

    pub const fn trimmed(decisions: Vec<Decision>) -> Self {
        Self {
            decisions,
            trimmed: true,
        }
    }
}

/// A System One backend that decides how to resolve conflicting chunks.
///
/// Implementations are used as `Box<dyn SystemOneProvider>`, so they are
/// written with `#[async_trait]`.
#[async_trait]
pub trait SystemOneProvider: Send + Sync {
    /// The name that selects this provider, such as `jev`.
    fn name(&self) -> &str;

    /// Identifies the provider, configured model, and prompt revision for the
    /// decision cache. Returning `None` disables caching for this provider.
    fn cache_key(&self) -> Option<String> {
        None
    }

    /// Returns one decision per entry of `state.chunks`, in the same order,
    /// along with whether hoisted signatures had to be trimmed to fit the
    /// provider's request size limit. A backend that cannot batch questions
    /// may ask about the chunks concurrently.
    async fn evaluate(&self, state: &SemanticState) -> Result<Evaluation, ProviderError>;
}

/// Asks `provider` about every chunk in `state`, giving up after `timeout`,
/// and checks that it answered each chunk exactly once.
pub async fn evaluate(
    provider: &dyn SystemOneProvider,
    state: &SemanticState,
    timeout: Duration,
) -> Result<Evaluation, ProviderError> {
    if state.chunks.is_empty() {
        return Ok(Evaluation::untrimmed(Vec::new()));
    }
    let evaluation = tokio::time::timeout(timeout, provider.evaluate(state))
        .await
        .map_err(|_| ProviderError::Timeout(timeout))??;
    if evaluation.decisions.len() != state.chunks.len() {
        return Err(ProviderError::InvalidResponse(format!(
            "decision count {} does not match chunk count {}",
            evaluation.decisions.len(),
            state.chunks.len()
        )));
    }
    Ok(evaluation)
}

/// One chunk's decision from [`evaluate_with_cache`], paired with whether it
/// came from the SQLite decision cache (`cached: true`) or a fresh provider
/// evaluation (`cached: false`).
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedDecision {
    pub decision: Decision,
    pub cached: bool,
}

impl ResolvedDecision {
    pub const fn new(decision: Decision, cached: bool) -> Self {
        Self { decision, cached }
    }
}

/// The outcome of [`evaluate_with_cache`] across all chunks in a
/// [`SemanticState`].
#[derive(Clone, Debug, PartialEq)]
pub struct BatchEvaluation {
    /// One entry per chunk in `state.chunks`, in order.
    /// `None` when a chunk missed in the cache and provider evaluation failed.
    pub decisions: Vec<Option<ResolvedDecision>>,
    /// Error or panic message if provider evaluation on cache misses failed.
    pub error: Option<String>,
}

/// Evaluates `state` through `provider` and the optional SQLite `cache`
/// (`ADR-0002`), querying only uncached chunks, skipping `Cache::put` when
/// hoisted signatures were trimmed (`Evaluation::trimmed`), and preserving
/// cached hits if the provider fails or panics on uncached misses.
pub fn evaluate_with_cache(
    provider: &dyn SystemOneProvider,
    state: &SemanticState,
    cache: Option<&Cache>,
    timeout: Duration,
) -> BatchEvaluation {
    let mut decisions = vec![None; state.chunks.len()];
    let cache_store = cache.and_then(|cache| provider.cache_key().map(|key| (cache, key)));
    let keys: Vec<Option<[u8; 32]>> = (0..state.chunks.len())
        .map(|position| {
            cache_store
                .as_ref()
                .map(|(_, provider_key)| cache::chunk_key(provider_key, state, position))
        })
        .collect();

    let mut misses = Vec::new();
    for position in 0..state.chunks.len() {
        if let (Some((cache, _)), Some(key)) = (&cache_store, &keys[position])
            && let Some(decision) = cache.get(key)
        {
            decisions[position] = Some(ResolvedDecision::new(decision, true));
        } else {
            misses.push(position);
        }
    }

    if misses.is_empty() {
        return BatchEvaluation {
            decisions,
            error: None,
        };
    }

    let name = provider.name();
    let Ok(missed_state) = panic::catch_unwind(AssertUnwindSafe(|| state.select_chunks(&misses)))
    else {
        return BatchEvaluation {
            decisions,
            error: Some(format!("{name} evaluation failed unexpectedly")),
        };
    };

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return BatchEvaluation {
                decisions,
                error: Some(format!(
                    "{name} evaluation skipped: cannot start a runtime: {error}"
                )),
            };
        }
    };
    let evaluation = panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(evaluate(provider, &missed_state, timeout))
    }));
    runtime.shutdown_background();
    match evaluation {
        Ok(Ok(evaluation)) => {
            for (&position, decision) in misses.iter().zip(evaluation.decisions) {
                if !evaluation.trimmed
                    && let (Some((cache, _)), Some(key)) = (&cache_store, &keys[position])
                {
                    cache.put(key, &decision);
                }
                decisions[position] = Some(ResolvedDecision::new(decision, false));
            }
            BatchEvaluation {
                decisions,
                error: None,
            }
        }
        Ok(Err(error)) => BatchEvaluation {
            decisions,
            error: Some(format!("{name} evaluation failed: {error}")),
        },
        Err(_) => BatchEvaluation {
            decisions,
            error: Some(format!("{name} evaluation failed unexpectedly")),
        },
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LoadError {
    Unknown(String),
    Unavailable {
        provider: &'static str,
        error: ProviderError,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(
                formatter,
                "unknown provider `{name}` (available: {})",
                PROVIDERS.join(", ")
            ),
            Self::Unavailable { provider, error } => {
                write!(formatter, "provider {provider} is unavailable: {error}")
            }
        }
    }
}

impl Error for LoadError {}

/// Loads the provider called `name`, ignoring ASCII case. `manual` loads none.
pub fn load(name: &str, env: Env) -> Result<Option<Box<dyn SystemOneProvider>>, LoadError> {
    fn boxed<P: SystemOneProvider + 'static>(
        provider: &'static str,
        loaded: Result<P, ProviderError>,
    ) -> Result<Option<Box<dyn SystemOneProvider>>, LoadError> {
        match loaded {
            Ok(loaded) => Ok(Some(Box::new(loaded))),
            Err(error) => Err(LoadError::Unavailable { provider, error }),
        }
    }

    if name.eq_ignore_ascii_case(MANUAL) {
        Ok(None)
    } else if name.eq_ignore_ascii_case(JevProvider::NAME) {
        boxed(JevProvider::NAME, JevProvider::from_env(env))
    } else if name.eq_ignore_ascii_case(MockProvider::NAME) {
        boxed(MockProvider::NAME, MockProvider::from_env(env))
    } else if name.eq_ignore_ascii_case(OllamaProvider::NAME) {
        boxed(OllamaProvider::NAME, OllamaProvider::from_env(env))
    } else {
        Err(LoadError::Unknown(name.to_owned()))
    }
}

// reqwest keeps the actual cause, such as "connection refused", in `source()`.
fn error_chain(error: &dyn Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = cause.source();
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::{ChunkState, Language, SignatureState, VersionState};
    use std::{
        collections::{HashMap, VecDeque},
        fs,
        sync::Mutex,
    };
    use tempfile::tempdir;

    fn confidence(value: f64) -> Confidence {
        Confidence::new(value).unwrap()
    }

    fn state(chunks: usize) -> SemanticState {
        SemanticState {
            file: "src/lib.rs".to_owned(),
            language: Language::Rust,
            chunks: (0..chunks)
                .map(|index| ChunkState {
                    items: vec![format!("fn item{index}")],
                    summary: String::new(),
                    base: None,
                    ours: None,
                    theirs: None,
                })
                .collect(),
            signatures: Vec::new(),
        }
    }

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let values: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn decisions_round_trip_through_the_documented_json_shape() {
        let decision = Decision::new(MergeAction::TakeOurs, confidence(0.98), "ours supersedes");
        let json = serde_json::to_value(&decision).unwrap();

        assert_eq!(
            json,
            json!({"decision": "TakeOurs", "confidence": 0.98, "reason": "ours supersedes"})
        );
        assert_eq!(serde_json::from_value::<Decision>(json).unwrap(), decision);
        for action in MergeAction::ALL {
            assert_eq!(
                serde_json::to_value(action).unwrap(),
                json!(action.as_str())
            );
            assert_eq!(action.as_str().parse::<MergeAction>(), Ok(action));
        }
    }

    #[test]
    fn rejects_unknown_decisions_and_confidence_outside_zero_to_one() {
        let parse = |text: &str| {
            serde_json::from_str::<Decision>(text)
                .unwrap_err()
                .to_string()
        };

        assert!(
            parse(r#"{"decision": "TakeBoth", "confidence": 0.9, "reason": ""}"#)
                .starts_with("unknown variant `TakeBoth`")
        );
        assert!(
            parse(r#"{"decision": "TakeOurs", "confidence": 1.5, "reason": ""}"#)
                .starts_with("confidence 1.5 is not between 0 and 1")
        );
        assert!(
            parse(r#"{"decision": "TakeOurs", "confidence": -0.1, "reason": ""}"#)
                .starts_with("confidence -0.1 is not between 0 and 1")
        );
        assert!(
            parse(r#"["TakeOurs", 0.9, ""]"#)
                .starts_with("invalid type: sequence, expected a decision object")
        );
        assert!(
            parse(r#"{"decision": "TakeOurs", "confidence": 0.9, "reason": "", "x": 1}"#)
                .starts_with("unknown field `x`")
        );
        assert_eq!(
            "takeours".parse::<MergeAction>().unwrap_err().to_string(),
            "unknown decision `takeours` (expected TakeOurs, TakeTheirs, or Manual)"
        );
        assert_eq!(Confidence::new(f64::NAN).map(f64::from).ok(), None);
    }

    #[test]
    fn the_json_schema_accepts_exactly_the_decisions_that_deserialize() {
        let schema = Decision::json_schema();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let documents = [
            (
                json!({"decision": "TakeOurs", "confidence": 0.98, "reason": "r"}),
                true,
            ),
            (
                json!({"decision": "TakeTheirs", "confidence": 1, "reason": ""}),
                true,
            ),
            (
                json!({"decision": "Manual", "confidence": 0, "reason": "unsure"}),
                true,
            ),
            (
                json!({"decision": "TakeBoth", "confidence": 0.5, "reason": ""}),
                false,
            ),
            (
                json!({"decision": "takeours", "confidence": 0.5, "reason": ""}),
                false,
            ),
            (
                json!({"decision": "TakeOurs", "confidence": 1.01, "reason": ""}),
                false,
            ),
            (
                json!({"decision": "TakeOurs", "confidence": -1, "reason": ""}),
                false,
            ),
            (
                json!({"decision": "TakeOurs", "confidence": "0.9", "reason": ""}),
                false,
            ),
            (
                json!({"decision": "TakeOurs", "confidence": null, "reason": ""}),
                false,
            ),
            (json!({"decision": "TakeOurs", "confidence": 0.9}), false),
            (json!({"decision": "TakeOurs", "reason": ""}), false),
            (json!({"confidence": 0.9, "reason": ""}), false),
            (
                json!({"decision": "TakeOurs", "confidence": 0.9, "reason": 7}),
                false,
            ),
            (
                json!({"decision": "TakeOurs", "confidence": 0.9, "reason": "", "extra": 1}),
                false,
            ),
            (json!(["TakeOurs", 0.9, ""]), false),
            (json!("TakeOurs"), false),
        ];

        for (document, valid) in documents {
            assert_eq!(validator.is_valid(&document), valid, "schema: {document}");
            assert_eq!(
                serde_json::from_value::<Decision>(document.clone()).is_ok(),
                valid,
                "deserializer: {document}"
            );
        }
    }

    #[test]
    fn only_confident_take_decisions_are_auto_resolvable() {
        let decision = |action, value| Decision::new(action, confidence(value), "");

        assert!(decision(MergeAction::TakeOurs, 0.951).is_auto_resolvable());
        assert!(decision(MergeAction::TakeTheirs, 1.0).is_auto_resolvable());
        assert!(!decision(MergeAction::TakeOurs, AUTO_RESOLVE_THRESHOLD).is_auto_resolvable());
        assert!(!decision(MergeAction::TakeTheirs, 0.5).is_auto_resolvable());
        assert!(!decision(MergeAction::Manual, 1.0).is_auto_resolvable());
    }

    struct Fixed(Vec<Decision>);

    #[async_trait]
    impl SystemOneProvider for Fixed {
        fn name(&self) -> &str {
            "fixed"
        }

        async fn evaluate(&self, _: &SemanticState) -> Result<Evaluation, ProviderError> {
            Ok(Evaluation::untrimmed(self.0.clone()))
        }
    }

    struct Slow;

    #[async_trait]
    impl SystemOneProvider for Slow {
        fn name(&self) -> &str {
            "slow"
        }

        async fn evaluate(&self, _: &SemanticState) -> Result<Evaluation, ProviderError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Evaluation::untrimmed(Vec::new()))
        }
    }

    #[tokio::test]
    async fn evaluates_through_dynamic_dispatch() {
        let decision = Decision::new(MergeAction::TakeTheirs, confidence(0.99), "theirs");
        let provider: Box<dyn SystemOneProvider> = Box::new(Fixed(vec![decision.clone(); 2]));

        let evaluation = evaluate(provider.as_ref(), &state(2), Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(provider.name(), "fixed");
        assert_eq!(
            evaluation,
            Evaluation::untrimmed(vec![decision.clone(), decision])
        );
    }

    #[tokio::test]
    async fn rejects_a_decision_count_that_does_not_match_the_conflicts() {
        let decision = Decision::new(MergeAction::TakeTheirs, confidence(0.99), "theirs");

        let error = evaluate(&Fixed(vec![decision]), &state(2), Duration::from_secs(5))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid response: decision count 1 does not match chunk count 2"
        );
        assert_eq!(
            evaluate(&Slow, &state(0), Duration::from_millis(1)).await,
            Ok(Evaluation::untrimmed(Vec::new()))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_when_the_provider_does_not_answer_in_time() {
        let error = evaluate(&Slow, &state(1), Duration::from_millis(250))
            .await
            .unwrap_err();

        assert_eq!(error, ProviderError::Timeout(Duration::from_millis(250)));
        assert_eq!(error.to_string(), "no answer within 250ms");
    }

    #[test]
    fn loads_providers_by_name() {
        let configured = env(&[
            ("TYPESAFE_API_KEY", "ts-key"),
            ("OLLAMA_MODEL", "kev:0.8b-bf16"),
            (
                MockProvider::ENV,
                r#"{"decision": "Manual", "confidence": 1, "reason": ""}"#,
            ),
        ]);
        let empty = env(&[]);

        assert!(load("manual", &empty).unwrap().is_none());
        assert!(load("Manual", &empty).unwrap().is_none());
        assert_eq!(load("jev", &configured).unwrap().unwrap().name(), "jev");
        assert_eq!(load("JEV", &configured).unwrap().unwrap().name(), "jev");
        assert_eq!(load("mock", &configured).unwrap().unwrap().name(), "mock");
        assert_eq!(
            load("ollama", &configured).unwrap().unwrap().name(),
            "ollama"
        );
        assert_eq!(
            load("OLLAMA", &configured).unwrap().unwrap().name(),
            "ollama"
        );

        let error = load("jev", &empty).err().unwrap();
        assert_eq!(
            error.to_string(),
            "provider jev is unavailable: TYPESAFE_API_KEY is not set"
        );
        let error = load("mock", &empty).err().unwrap();
        assert_eq!(
            error.to_string(),
            "provider mock is unavailable: WELD_MOCK_DECISION is not set"
        );
        let error = load("ollama", &empty).err().unwrap();
        assert_eq!(
            error.to_string(),
            "provider ollama is unavailable: OLLAMA_MODEL is not set"
        );
        for name in ["clef", "sap-rst", "jev2"] {
            let error = load(name, &configured).err().unwrap();
            assert_eq!(error, LoadError::Unknown(name.to_owned()));
            assert_eq!(
                error.to_string(),
                format!("unknown provider `{name}` (available: jev, manual, mock, ollama)")
            );
        }
    }

    enum StubReply {
        Ok(Evaluation),
        Err(ProviderError),
        Panic(&'static str),
    }

    struct StubProvider {
        cache_key: Option<String>,
        replies: Mutex<VecDeque<StubReply>>,
        seen_states: Mutex<Vec<SemanticState>>,
    }

    impl StubProvider {
        fn new(cache_key: Option<&str>, replies: impl IntoIterator<Item = StubReply>) -> Self {
            Self {
                cache_key: cache_key.map(str::to_owned),
                replies: Mutex::new(replies.into_iter().collect()),
                seen_states: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> usize {
            self.seen_states.lock().unwrap().len()
        }

        fn seen_states(&self) -> Vec<SemanticState> {
            self.seen_states.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SystemOneProvider for StubProvider {
        fn name(&self) -> &str {
            "stub"
        }

        fn cache_key(&self) -> Option<String> {
            self.cache_key.clone()
        }

        async fn evaluate(&self, state: &SemanticState) -> Result<Evaluation, ProviderError> {
            self.seen_states.lock().unwrap().push(state.clone());
            match self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected provider call")
            {
                StubReply::Ok(evaluation) => Ok(evaluation),
                StubReply::Err(error) => Err(error),
                StubReply::Panic(message) => panic!("{message}"),
            }
        }
    }

    fn version_state(code: &str) -> Option<VersionState> {
        Some(VersionState {
            lines: 1..=3,
            code: code.to_owned(),
            changes: None,
        })
    }

    fn three_chunk_state() -> SemanticState {
        SemanticState {
            file: "src/lib.rs".to_owned(),
            language: Language::Rust,
            chunks: ["alpha", "beta", "gamma"]
                .into_iter()
                .enumerate()
                .map(|(index, name)| ChunkState {
                    items: vec![format!("fn {name}")],
                    summary: format!("chunk {index}"),
                    base: version_state(&format!("fn {name}() -> u32 {{ {index} }}")),
                    ours: version_state(&format!("fn {name}() -> u32 {{ {} }}", index + 10)),
                    theirs: version_state(&format!("fn {name}() -> u32 {{ {} }}", index + 100)),
                })
                .collect(),
            signatures: vec![
                SignatureState {
                    item: "struct Shared".to_owned(),
                    signature: Some("struct Shared {\n    id: u64,\n}".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0, 1],
                },
                SignatureState {
                    item: "struct AlphaOnly".to_owned(),
                    signature: Some("struct AlphaOnly;".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "struct BetaOnly".to_owned(),
                    signature: Some("struct BetaOnly;".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![1],
                },
            ],
        }
    }

    #[test]
    fn evaluate_with_cache_records_misses_and_serves_full_hits_without_calling_provider() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("git-weld/cache")).unwrap();
        let full_state = three_chunk_state().select_chunks(&[0, 1]);
        let d0 = Decision::new(MergeAction::TakeOurs, confidence(0.99), "ours 0");
        let d1 = Decision::new(MergeAction::Manual, confidence(0.70), "manual 1");

        let first_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Ok(Evaluation::untrimmed(vec![
                d0.clone(),
                d1.clone(),
            ]))],
        );
        let first = evaluate_with_cache(
            &first_provider,
            &full_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(first_provider.calls(), 1);
        assert_eq!(
            first,
            BatchEvaluation {
                decisions: vec![
                    Some(ResolvedDecision::new(d0.clone(), false)),
                    Some(ResolvedDecision::new(d1.clone(), false)),
                ],
                error: None,
            }
        );

        let hit_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Panic("full cache hit must not call provider")],
        );
        let second = evaluate_with_cache(
            &hit_provider,
            &full_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(hit_provider.calls(), 0);
        assert_eq!(
            second,
            BatchEvaluation {
                decisions: vec![
                    Some(ResolvedDecision::new(d0, true)),
                    Some(ResolvedDecision::new(d1, true)),
                ],
                error: None,
            }
        );
    }

    #[test]
    fn evaluate_with_cache_subsets_partial_misses_and_reassembles_decisions_in_chunk_order() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("git-weld/cache")).unwrap();
        let state = three_chunk_state();
        let d_alpha = Decision::new(MergeAction::TakeOurs, confidence(0.99), "alpha");
        let d_beta = Decision::new(MergeAction::Manual, confidence(0.65), "beta");
        let d_gamma = Decision::new(MergeAction::TakeTheirs, confidence(0.98), "gamma");

        // Seed chunks 0 (`fn alpha`) and 2 (`fn gamma`) into the SQLite cache.
        let seeded_state = state.select_chunks(&[0, 2]);
        let seed_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Ok(Evaluation::untrimmed(vec![
                d_alpha.clone(),
                d_gamma.clone(),
            ]))],
        );
        let seeded = evaluate_with_cache(
            &seed_provider,
            &seeded_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(seeded.error, None);
        assert_eq!(seed_provider.calls(), 1);

        // Evaluating all 3 chunks sends only chunk 1 (`fn beta`) and its referenced
        // signatures (`struct Shared` and `struct BetaOnly`, remapped to index 0).
        let miss_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Ok(Evaluation::untrimmed(vec![d_beta.clone()]))],
        );
        let partial =
            evaluate_with_cache(&miss_provider, &state, Some(&cache), Duration::from_secs(5));
        assert_eq!(miss_provider.calls(), 1);
        let seen = &miss_provider.seen_states()[0];
        assert_eq!(seen.chunks.len(), 1);
        assert_eq!(seen.chunks[0].items, ["fn beta"]);
        assert_eq!(
            seen.signatures,
            vec![
                SignatureState {
                    item: "struct Shared".to_owned(),
                    signature: Some("struct Shared {\n    id: u64,\n}".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
                SignatureState {
                    item: "struct BetaOnly".to_owned(),
                    signature: Some("struct BetaOnly;".to_owned()),
                    base: None,
                    ours: None,
                    theirs: None,
                    chunks: vec![0],
                },
            ]
        );
        assert_eq!(
            partial,
            BatchEvaluation {
                decisions: vec![
                    Some(ResolvedDecision::new(d_alpha.clone(), true)),
                    Some(ResolvedDecision::new(d_beta.clone(), false)),
                    Some(ResolvedDecision::new(d_gamma.clone(), true)),
                ],
                error: None,
            }
        );

        // All 3 chunks are now cached.
        let verify_provider = StubProvider::new(Some("stub:model:v1"), []);
        let all_cached = evaluate_with_cache(
            &verify_provider,
            &state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(verify_provider.calls(), 0);
        assert_eq!(
            all_cached,
            BatchEvaluation {
                decisions: vec![
                    Some(ResolvedDecision::new(d_alpha, true)),
                    Some(ResolvedDecision::new(d_beta, true)),
                    Some(ResolvedDecision::new(d_gamma, true)),
                ],
                error: None,
            }
        );
    }

    #[test]
    fn evaluate_with_cache_preserves_cached_decisions_when_provider_errors_or_panics_on_misses() {
        let dir = tempdir().unwrap();
        let cache = Cache::open(&dir.path().join("git-weld/cache")).unwrap();
        let state = three_chunk_state();
        let d_alpha = Decision::new(MergeAction::TakeOurs, confidence(0.99), "alpha");
        let d_gamma = Decision::new(MergeAction::TakeTheirs, confidence(0.98), "gamma");

        let seed_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Ok(Evaluation::untrimmed(vec![
                d_alpha.clone(),
                d_gamma.clone(),
            ]))],
        );
        evaluate_with_cache(
            &seed_provider,
            &state.select_chunks(&[0, 2]),
            Some(&cache),
            Duration::from_secs(5),
        );

        let failing_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Err(ProviderError::Http {
                status: 503,
                message: "service unavailable".to_owned(),
            })],
        );
        let failed = evaluate_with_cache(
            &failing_provider,
            &state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(
            failed,
            BatchEvaluation {
                decisions: vec![
                    Some(ResolvedDecision::new(d_alpha.clone(), true)),
                    None,
                    Some(ResolvedDecision::new(d_gamma.clone(), true)),
                ],
                error: Some("stub evaluation failed: HTTP 503: service unavailable".to_owned()),
            }
        );

        let panicking_provider =
            StubProvider::new(Some("stub:model:v1"), [StubReply::Panic("boom")]);
        let panicked = evaluate_with_cache(
            &panicking_provider,
            &state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(
            panicked,
            BatchEvaluation {
                decisions: vec![
                    Some(ResolvedDecision::new(d_alpha, true)),
                    None,
                    Some(ResolvedDecision::new(d_gamma, true)),
                ],
                error: Some("stub evaluation failed unexpectedly".to_owned()),
            }
        );
    }

    #[test]
    fn evaluate_with_cache_bypasses_writes_for_trimmed_or_unkeyed_providers_and_degrades_on_cache_errors()
     {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("git-weld/cache");
        let cache = Cache::open(&db_path).unwrap();
        let single_state = three_chunk_state().select_chunks(&[0]);
        let d0 = Decision::new(MergeAction::TakeOurs, confidence(0.99), "alpha");

        // ADR-0002: trimmed evaluations return fresh decisions (`cached: false`)
        // without storing them in SQLite.
        let trimmed_provider = StubProvider::new(
            Some("stub:model:v1"),
            [
                StubReply::Ok(Evaluation::trimmed(vec![d0.clone()])),
                StubReply::Ok(Evaluation::untrimmed(vec![d0.clone()])),
            ],
        );
        let first = evaluate_with_cache(
            &trimmed_provider,
            &single_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(
            first,
            BatchEvaluation {
                decisions: vec![Some(ResolvedDecision::new(d0.clone(), false))],
                error: None,
            }
        );
        let second = evaluate_with_cache(
            &trimmed_provider,
            &single_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(trimmed_provider.calls(), 2);
        assert_eq!(
            second,
            BatchEvaluation {
                decisions: vec![Some(ResolvedDecision::new(d0.clone(), false))],
                error: None,
            }
        );
        let third = evaluate_with_cache(
            &trimmed_provider,
            &single_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(trimmed_provider.calls(), 2);
        assert_eq!(
            third,
            BatchEvaluation {
                decisions: vec![Some(ResolvedDecision::new(d0.clone(), true))],
                error: None,
            }
        );

        // Providers with `cache_key() == None` never read or write the SQLite cache.
        let unkeyed = StubProvider::new(
            None,
            [
                StubReply::Ok(Evaluation::untrimmed(vec![d0.clone()])),
                StubReply::Ok(Evaluation::untrimmed(vec![d0.clone()])),
            ],
        );
        for _ in 0..2 {
            let outcome = evaluate_with_cache(
                &unkeyed,
                &single_state,
                Some(&cache),
                Duration::from_secs(5),
            );
            assert_eq!(
                outcome,
                BatchEvaluation {
                    decisions: vec![Some(ResolvedDecision::new(d0.clone(), false))],
                    error: None,
                }
            );
        }
        assert_eq!(unkeyed.calls(), 2);

        // Corrupt or damaged SQLite state degrades gracefully to uncached evaluation.
        let bad_path = dir.path().join("corrupt-cache");
        fs::write(&bad_path, b"not a sqlite file").unwrap();
        let reopened = Cache::open(&bad_path);
        assert!(reopened.is_none());

        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute("DROP TABLE decisions", [])
            .unwrap();
        let fallback_provider = StubProvider::new(
            Some("stub:model:v1"),
            [StubReply::Ok(Evaluation::untrimmed(vec![d0.clone()]))],
        );
        let degraded = evaluate_with_cache(
            &fallback_provider,
            &single_state,
            Some(&cache),
            Duration::from_secs(5),
        );
        assert_eq!(fallback_provider.calls(), 1);
        assert_eq!(
            degraded,
            BatchEvaluation {
                decisions: vec![Some(ResolvedDecision::new(d0, false))],
                error: None,
            }
        );
    }
}
