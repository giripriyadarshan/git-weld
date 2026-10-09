//! The model-agnostic System One layer: the uniform [`Decision`] every
//! provider returns, the [`SystemOneProvider`] trait, and loading a provider
//! by the name given to `--provider`.

mod jev;
mod mock;

pub use jev::JevProvider;
pub use mock::MockProvider;

use crate::state::SemanticState;
use async_trait::async_trait;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};
use serde_json::{Value, json};
use std::{error::Error, fmt, str::FromStr, time::Duration};

/// A decision may resolve its item without a person only when its confidence
/// is strictly greater than this. A confidence of exactly 0.95 does not.
pub const AUTO_RESOLVE_THRESHOLD: f64 = 0.95;

/// The provider name that turns model evaluation off.
pub const MANUAL: &str = "manual";

/// Every name that [`load`] accepts.
pub const PROVIDERS: [&str; 3] = [JevProvider::NAME, MANUAL, MockProvider::NAME];

/// Looks up configuration such as API keys; `std::env::var` in the driver.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What to do with one item that both sides changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MergeAction {
    /// Keep our version of the item.
    TakeOurs,
    /// Keep their version of the item.
    TakeTheirs,
    /// Neither version alone is right, so a person must merge the item.
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

/// The uniform answer for one conflicting item, whatever the backend:
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
            "description": "How to resolve one item that both sides of a merge changed.",
            "type": "object",
            "properties": {
                "decision": {
                    "description": "Keep our version, keep their version, or leave the item to a person.",
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

    /// Whether the decision is confident enough to resolve its item without
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

/// A System One backend that decides how to resolve conflicting items.
///
/// Implementations are used as `Box<dyn SystemOneProvider>`, so they are
/// written with `#[async_trait]`.
#[async_trait]
pub trait SystemOneProvider: Send + Sync {
    /// The name that selects this provider, such as `jev`.
    fn name(&self) -> &str;

    /// Returns one decision per entry of `state.conflicts`, in the same order.
    async fn evaluate(&self, state: &SemanticState) -> Result<Vec<Decision>, ProviderError>;
}

/// Asks `provider` about every conflict in `state`, giving up after
/// `timeout`, and checks that it answered each conflict exactly once.
pub async fn evaluate(
    provider: &dyn SystemOneProvider,
    state: &SemanticState,
    timeout: Duration,
) -> Result<Vec<Decision>, ProviderError> {
    if state.conflicts.is_empty() {
        return Ok(Vec::new());
    }
    let decisions = tokio::time::timeout(timeout, provider.evaluate(state))
        .await
        .map_err(|_| ProviderError::Timeout(timeout))??;
    if decisions.len() != state.conflicts.len() {
        return Err(ProviderError::InvalidResponse(format!(
            "decision count {} does not match conflicting item count {}",
            decisions.len(),
            state.conflicts.len()
        )));
    }
    Ok(decisions)
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
    use crate::{ast::Language, state::ConflictState};
    use std::collections::HashMap;

    fn confidence(value: f64) -> Confidence {
        Confidence::new(value).unwrap()
    }

    fn state(conflicts: usize) -> SemanticState {
        SemanticState {
            file: "src/lib.rs".to_owned(),
            language: Language::Rust,
            conflicts: (0..conflicts)
                .map(|index| ConflictState {
                    item: format!("fn item{index}"),
                    node_type: "function_item".to_owned(),
                    summary: String::new(),
                    base: None,
                    ours: None,
                    theirs: None,
                })
                .collect(),
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

        async fn evaluate(&self, _: &SemanticState) -> Result<Vec<Decision>, ProviderError> {
            Ok(self.0.clone())
        }
    }

    struct Slow;

    #[async_trait]
    impl SystemOneProvider for Slow {
        fn name(&self) -> &str {
            "slow"
        }

        async fn evaluate(&self, _: &SemanticState) -> Result<Vec<Decision>, ProviderError> {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn evaluates_through_dynamic_dispatch() {
        let decision = Decision::new(MergeAction::TakeTheirs, confidence(0.99), "theirs");
        let provider: Box<dyn SystemOneProvider> = Box::new(Fixed(vec![decision.clone(); 2]));

        let decisions = evaluate(provider.as_ref(), &state(2), Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(provider.name(), "fixed");
        assert_eq!(decisions, [decision.clone(), decision]);
    }

    #[tokio::test]
    async fn rejects_a_decision_count_that_does_not_match_the_conflicts() {
        let decision = Decision::new(MergeAction::TakeTheirs, confidence(0.99), "theirs");

        let error = evaluate(&Fixed(vec![decision]), &state(2), Duration::from_secs(5))
            .await
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "invalid response: decision count 1 does not match conflicting item count 2"
        );
        assert_eq!(
            evaluate(&Slow, &state(0), Duration::from_millis(1)).await,
            Ok(Vec::new())
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
        for name in ["clef", "sap-rst", "jev2"] {
            let error = load(name, &configured).err().unwrap();
            assert_eq!(error, LoadError::Unknown(name.to_owned()));
            assert_eq!(
                error.to_string(),
                format!("unknown provider `{name}` (available: jev, manual, mock)")
            );
        }
    }
}
