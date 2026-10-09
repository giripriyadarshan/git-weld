//! An offline provider for tests: it answers with decisions read from
//! `WELD_MOCK_DECISION` instead of asking a model.

use super::{Decision, Env, ProviderError, SystemOneProvider};
use crate::state::SemanticState;
use async_trait::async_trait;
use serde_json::Value;

/// Answers every conflict with the decision in its JSON configuration: one
/// decision object for every item, or an array with one decision per item.
/// The JSON is validated like a real provider's answer, on every evaluation.
#[derive(Clone, Debug)]
pub struct MockProvider {
    answer: String,
}

impl MockProvider {
    pub const NAME: &str = "mock";
    pub const ENV: &str = "WELD_MOCK_DECISION";

    pub fn new(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
        }
    }

    pub fn from_env(env: Env) -> Result<Self, ProviderError> {
        env(Self::ENV)
            .map(Self::new)
            .ok_or_else(|| ProviderError::Config(format!("{} is not set", Self::ENV)))
    }
}

#[async_trait]
impl SystemOneProvider for MockProvider {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn evaluate(&self, state: &SemanticState) -> Result<Vec<Decision>, ProviderError> {
        let invalid = |error: serde_json::Error| ProviderError::InvalidResponse(error.to_string());
        let count = state.conflicts.len();
        match serde_json::from_str(&self.answer).map_err(invalid)? {
            Value::Array(answers) if answers.len() != count => {
                Err(ProviderError::InvalidResponse(format!(
                    "{}: decision count {} does not match conflicting item count {count}",
                    Self::ENV,
                    answers.len()
                )))
            }
            Value::Array(answers) => answers
                .into_iter()
                .map(|answer| serde_json::from_value(answer).map_err(invalid))
                .collect(),
            answer => {
                let decision: Decision = serde_json::from_value(answer).map_err(invalid)?;
                Ok(vec![decision; count])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ast::Language,
        provider::{Confidence, MergeAction},
        state::ConflictState,
    };

    fn state(conflicts: usize) -> SemanticState {
        SemanticState {
            file: "lib.rs".to_owned(),
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

    fn decision(action: MergeAction, confidence: f64, reason: &str) -> Decision {
        Decision::new(action, Confidence::new(confidence).unwrap(), reason)
    }

    #[tokio::test]
    async fn answers_every_conflict_with_one_decision() {
        let mock = MockProvider::new(
            r#"{"decision": "TakeTheirs", "confidence": 0.99, "reason": "theirs wins"}"#,
        );

        let decisions = mock.evaluate(&state(2)).await.unwrap();

        let expected = decision(MergeAction::TakeTheirs, 0.99, "theirs wins");
        assert_eq!(decisions, [expected.clone(), expected]);
    }

    #[tokio::test]
    async fn answers_each_conflict_from_an_array() {
        let mock = MockProvider::new(
            r#"[
                {"decision": "TakeOurs", "confidence": 1, "reason": "a"},
                {"decision": "Manual", "confidence": 0.4, "reason": "b"}
            ]"#,
        );

        assert_eq!(
            mock.evaluate(&state(2)).await.unwrap(),
            [
                decision(MergeAction::TakeOurs, 1.0, "a"),
                decision(MergeAction::Manual, 0.4, "b")
            ]
        );
        assert_eq!(
            mock.evaluate(&state(3)).await.unwrap_err().to_string(),
            "invalid response: WELD_MOCK_DECISION: decision count 2 does not match \
             conflicting item count 3"
        );
    }

    #[tokio::test]
    async fn rejects_answers_that_break_the_schema() {
        for (answer, message) in [
            (
                "not json",
                "invalid response: expected ident at line 1 column 2",
            ),
            (
                r#"{"decision": "TakeBoth", "confidence": 0.99, "reason": ""}"#,
                "invalid response: unknown variant `TakeBoth`, expected one of \
                 `TakeOurs`, `TakeTheirs`, `Manual`",
            ),
            (
                r#"[{"decision": "TakeOurs", "confidence": 2, "reason": ""}]"#,
                "invalid response: confidence 2 is not between 0 and 1",
            ),
        ] {
            let error = MockProvider::new(answer)
                .evaluate(&state(1))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), message, "{answer}");
        }
    }

    #[test]
    fn reads_its_answer_from_the_environment() {
        let set = |key: &str| (key == MockProvider::ENV).then(|| "{}".to_owned());
        let unset = |_: &str| None;

        assert_eq!(MockProvider::from_env(&set).unwrap().answer, "{}");
        assert_eq!(
            MockProvider::from_env(&unset).unwrap_err(),
            ProviderError::Config("WELD_MOCK_DECISION is not set".to_owned())
        );
    }
}
