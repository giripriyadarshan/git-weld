mod support;

use git_weld::{
    ast,
    merge::Versions,
    provider::{self, Confidence, Decision, JevProvider, MergeAction, ProviderError},
    state::SemanticState,
};
use serde_json::json;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use support::{Response, Server, closed_url};

fn state() -> SemanticState {
    let base = "fn alpha() -> u32 {\n    1\n}\n";
    let versions = Versions {
        base: base.as_bytes().to_vec(),
        ours: base.replace("    1\n", "    10\n").into_bytes(),
        theirs: base.replace("    1\n", "    100\n").into_bytes(),
    };
    let path = Path::new("src/lib.rs");
    let analysis = ast::analyze(path, &versions.base, &versions.ours, &versions.theirs).unwrap();
    SemanticState::new(path, &analysis, &versions)
}

fn jev(url: &str) -> JevProvider {
    JevProvider::new(url, "ts-test-key", "jev-latest")
        .unwrap()
        .with_retries(2, Duration::from_millis(10))
}

fn answer(choice: &str, confidence: f64) -> String {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "conflict_0": {
                "type": "choice",
                "choice": choice,
                "confidence": confidence,
                "probabilities": {"Manual": 0.01, "TakeOurs": 0.01, "TakeTheirs": 0.98},
            },
        },
        "usage": {"input_tokens": 420, "output_tokens": 30},
    })
    .to_string()
}

async fn evaluate(jev: &JevProvider) -> Result<Vec<Decision>, ProviderError> {
    provider::evaluate(jev, &state(), Duration::from_secs(10)).await
}

#[tokio::test]
async fn posts_the_state_with_one_question_per_conflict_and_returns_decisions() {
    let server = Server::start(vec![Response::new(200, answer("TakeTheirs", 0.97))]);

    let decisions = evaluate(&jev(&server.url())).await.unwrap();

    assert_eq!(
        decisions,
        [Decision::new(
            MergeAction::TakeTheirs,
            Confidence::new(0.97).unwrap(),
            "jev-1.13.0 chose TakeTheirs \
             (probabilities: Manual 0.01, TakeOurs 0.01, TakeTheirs 0.98)"
        )]
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/systemone");
    assert_eq!(request.header("authorization"), Some("Bearer ts-test-key"));
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert!(
        request
            .header("user-agent")
            .is_some_and(|agent| agent.starts_with("git-weld/"))
    );
    let body = request.json();
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], serde_json::to_value(state()).unwrap());
    assert_eq!(body["questions"].as_object().unwrap().len(), 1);
    assert_eq!(body["questions"]["conflict_0"]["type"], "choice");
}

#[tokio::test]
async fn reports_client_errors_without_retrying() {
    let server = Server::start(vec![
        Response::new(401, r#"{"detail": "Invalid API key"}"#),
        Response::new(422, "{\n  \"detail\": \"questions: field required\"\n}"),
    ]);
    let jev = jev(&server.url());

    let unauthorized = evaluate(&jev).await.unwrap_err();
    let unprocessable = evaluate(&jev).await.unwrap_err();

    assert_eq!(
        unauthorized.to_string(),
        r#"HTTP 401: {"detail": "Invalid API key"} (check TYPESAFE_API_KEY)"#
    );
    assert_eq!(
        unprocessable,
        ProviderError::Http {
            status: 422,
            message: r#"{ "detail": "questions: field required" }"#.to_owned(),
        }
    );
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn retries_rate_limits_and_overload() {
    let server = Server::start(vec![
        Response::new(429, "{}").header("retry-after-ms", "5"),
        Response::new(529, r#"{"detail": "Overloaded"}"#),
        Response::new(200, answer("TakeOurs", 0.99)),
    ]);

    let decisions = evaluate(&jev(&server.url())).await.unwrap();

    assert_eq!(decisions[0].decision, MergeAction::TakeOurs);
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn gives_up_after_the_last_retry() {
    let server = Server::start(vec![
        Response::new(500, "boom"),
        Response::new(503, "still down"),
        Response::new(502, "bad gateway"),
    ]);

    let error = evaluate(&jev(&server.url())).await.unwrap_err();

    assert_eq!(
        error,
        ProviderError::Http {
            status: 502,
            message: "bad gateway".to_owned(),
        }
    );
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn does_not_wait_when_asked_to_retry_much_later() {
    let server = Server::start(vec![
        Response::new(429, r#"{"detail": "Rate limited"}"#).header("retry-after", "120"),
    ]);
    let started = Instant::now();

    let error = evaluate(&jev(&server.url())).await.unwrap_err();

    assert_eq!(error.to_string(), r#"HTTP 429: {"detail": "Rate limited"}"#);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn rejects_answers_outside_the_schema() {
    let server = Server::start(vec![
        Response::new(200, answer("TakeBoth", 0.99)),
        Response::new(200, answer("TakeOurs", 1.5)),
        Response::new(200, "<html>proxy error</html>"),
    ]);
    let jev = jev(&server.url());

    let mut errors = Vec::new();
    for _ in 0..3 {
        errors.push(evaluate(&jev).await.unwrap_err().to_string());
    }

    assert_eq!(
        errors,
        [
            "invalid response: answers.conflict_0.choice: unknown decision `TakeBoth` \
             (expected TakeOurs, TakeTheirs, or Manual)",
            "invalid response: answers.conflict_0.confidence: \
             confidence 1.5 is not between 0 and 1",
            "invalid response: expected value at line 1 column 1",
        ]
    );
}

#[tokio::test]
async fn times_out_when_the_service_is_slow() {
    let server = Server::start(vec![
        Response::new(200, answer("TakeOurs", 0.99)).delay(Duration::from_secs(3)),
    ]);
    let started = Instant::now();

    let error = provider::evaluate(&jev(&server.url()), &state(), Duration::from_millis(200))
        .await
        .unwrap_err();

    assert_eq!(error, ProviderError::Timeout(Duration::from_millis(200)));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn reports_refused_connections_after_retrying() {
    let error = evaluate(&jev(&closed_url())).await.unwrap_err();

    let ProviderError::Transport(message) = error else {
        panic!("expected a transport error, got {error:?}");
    };
    assert!(message.contains("error sending request"), "{message}");
}
