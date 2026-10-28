mod support;

use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
use support::{Response, Server};
use tempfile::{TempDir, tempdir};

// Settings that would let the caller's environment leak into a test.
const SETTINGS: [&str; 10] = [
    "WELD_PROVIDER",
    "WELD_TIMEOUT",
    "WELD_NO_CACHE",
    "WELD_MOCK_DECISION",
    "TYPESAFE_API_KEY",
    "TYPESAFE_BASE_URL",
    "TYPESAFE_DEFAULT_MODEL",
    "OLLAMA_MODEL",
    "OLLAMA_HOST",
    "OLLAMA_API_KEY",
];

const RUST_BASE: &str = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";
const THREE: &str = "\
fn alpha() -> u32 {
    1
}

fn beta() -> u32 {
    2
}

fn gamma() -> u32 {
    3
}
";

struct MergeFixture {
    directory: TempDir,
}

impl MergeFixture {
    fn new() -> Self {
        Self::with_contents("base\n", "ours\n", "theirs\n")
    }

    fn with_contents(base: &str, ours: &str, theirs: &str) -> Self {
        let fixture = Self {
            directory: tempdir().unwrap(),
        };
        fs::write(fixture.path("base"), base).unwrap();
        fs::write(fixture.path("ours"), ours).unwrap();
        fs::write(fixture.path("theirs"), theirs).unwrap();
        fixture
    }

    // `fn alpha` conflicts; with `both`, `fn beta` does too.
    fn rust_conflict(both: bool) -> Self {
        let (ours, theirs) = rust_versions(both);
        Self::with_contents(RUST_BASE, &ours, &theirs)
    }

    fn three_conflicts() -> Self {
        let edit = |suffix: &str| {
            THREE
                .replace("    1\n", &format!("    1{suffix}\n"))
                .replace("    2\n", &format!("    2{suffix}\n"))
                .replace("    3\n", &format!("    3{suffix}\n"))
        };
        Self::with_contents(THREE, &edit("0"), &edit("1"))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn command(&self) -> Command {
        self.command_for("source file.rs")
    }

    // Runs in the fixture directory, where the driver writes `.git-weld.log`.
    fn command_for(&self, logical_path: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-weld"));
        for setting in SETTINGS {
            command.env_remove(setting);
        }
        command
            .current_dir(self.directory.path())
            .arg("-o")
            .arg(self.path("base"))
            .arg("-a")
            .arg(self.path("ours"))
            .arg("-b")
            .arg(self.path("theirs"))
            .args(["-p", logical_path]);
        command
    }

    fn mock_command(&self, decision: &str) -> Command {
        self.mock_command_for("src/lib.rs", decision)
    }

    fn mock_command_for(&self, logical_path: &str, decision: &str) -> Command {
        let mut command = self.command_for(logical_path);
        command
            .args(["--provider", "mock"])
            .env("WELD_MOCK_DECISION", decision);
        command
    }

    fn jev_command_for(&self, logical_path: &str, url: &str) -> Command {
        let mut command = self.command_for(logical_path);
        command
            .args(["--provider", "jev"])
            .env("TYPESAFE_API_KEY", "ts-test-key")
            .env("TYPESAFE_BASE_URL", url)
            .env("TYPESAFE_DEFAULT_MODEL", "jev-1.13.0");
        command
    }

    fn result(&self) -> String {
        fs::read_to_string(self.path("ours")).unwrap()
    }

    fn log(&self) -> Vec<Value> {
        read_log(self.directory.path())
    }
}

fn rust_versions(both: bool) -> (String, String) {
    let mut ours = RUST_BASE.replace("    1\n", "    10\n");
    let mut theirs = RUST_BASE.replace("    1\n", "    100\n");
    if both {
        ours = ours.replace("    2\n", "    20\n");
        theirs = theirs.replace("    2\n", "    200\n");
    }
    (ours, theirs)
}

fn read_log(directory: &Path) -> Vec<Value> {
    match fs::read_to_string(directory.join(".git-weld.log")) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn conflict(marker: usize, ours: &str, base: &str, theirs: &str) -> String {
    let markers = |character: char| character.to_string().repeat(marker);
    format!(
        "{} HEAD\n{ours}{} base\n{base}{}\n{theirs}{} theirs\n",
        markers('<'),
        markers('|'),
        markers('='),
        markers('>')
    )
}

// What the line merge writes for `MergeFixture::rust_conflict`.
fn rust_conflict_result(both: bool) -> String {
    let beta = if both {
        format!(
            "fn beta() -> u32 {{\n{}}}\n",
            conflict(7, "    20\n", "    2\n", "    200\n")
        )
    } else {
        "fn beta() -> u32 {\n    2\n}\n".to_owned()
    };
    format!(
        "fn alpha() -> u32 {{\n{}}}\n\n{beta}",
        conflict(7, "    10\n", "    1\n", "    100\n")
    )
}

fn exit(output: Output) -> (Option<i32>, String) {
    assert!(output.stdout.is_empty(), "{output:?}");
    (
        output.status.code(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn decision(action: &str, confidence: f64, reason: &str) -> String {
    json!({"decision": action, "confidence": confidence, "reason": reason}).to_string()
}

fn jev_answers(choices: &[(&str, f64)]) -> String {
    let answers: serde_json::Map<String, Value> = choices
        .iter()
        .enumerate()
        .map(|(index, (choice, confidence))| {
            (
                format!("chunk_{index}"),
                json!({
                    "type": "choice",
                    "choice": choice,
                    "confidence": confidence,
                    "probabilities": {"Manual": 0.01, "TakeOurs": 0.99, "TakeTheirs": 0.0},
                }),
            )
        })
        .collect();
    json!({
        "model": "jev-1.13.0",
        "answers": answers,
        "usage": {"input_tokens": 512, "output_tokens": 34},
    })
    .to_string()
}

#[test]
fn cli_writes_line_level_conflicts_when_no_model_is_configured() {
    let fixture = MergeFixture::rust_conflict(true);

    let (code, diagnostics) = exit(fixture.command_for("src/lib.rs").output().unwrap());

    assert_eq!(code, Some(1));
    assert_eq!(
        diagnostics,
        "git-weld: src/lib.rs: 2 conflicts in 2 chunks (provider: manual):\n\
         git-weld:   chunk 1: fn alpha\n\
         git-weld:   chunk 2: fn beta\n\
         git-weld: 2 chunks left for manual resolution, at lines 1-9 and 11-19\n"
    );
    assert_eq!(fixture.result(), rust_conflict_result(true));
    assert!(fixture.log().is_empty());
}

#[test]
fn cli_merges_edits_that_do_not_overlap_without_a_model() {
    let base = "import os\n\n\ndef alpha():\n    return 1\n\n\ndef beta():\n    return 2\n";
    let ours = base.replace("return 1", "return 10");
    let theirs = base
        .replace("import os\n", "import os\nimport sys\n")
        .replace("return 2", "return 20");
    let fixture = MergeFixture::with_contents(base, &ours, &theirs);

    let (code, diagnostics) = exit(fixture.command_for("app.py").output().unwrap());

    assert_eq!(code, Some(0));
    assert_eq!(diagnostics, "");
    assert_eq!(
        fixture.result(),
        "import os\nimport sys\n\n\ndef alpha():\n    return 10\n\n\ndef beta():\n    return 20\n"
    );
}

#[test]
fn clean_merges_need_neither_a_supported_language_nor_valid_syntax() {
    for (path, base, ours, theirs, merged) in [
        (
            "notes.txt",
            "one\ntwo\nthree\n",
            "ONE\ntwo\nthree\n",
            "one\ntwo\nTHREE\n",
            "ONE\ntwo\nTHREE\n",
        ),
        (
            "lib.rs",
            "fn a() {}\n\nfn b() {}\n",
            "fn a( {}\n\nfn b() {}\n",
            "fn a() {}\n\nfn b() { 1 }\n",
            "fn a( {}\n\nfn b() { 1 }\n",
        ),
    ] {
        let fixture = MergeFixture::with_contents(base, ours, theirs);

        let (code, diagnostics) = exit(fixture.command_for(path).output().unwrap());

        assert_eq!((code, diagnostics.as_str()), (Some(0), ""), "{path}");
        assert_eq!(fixture.result(), merged);
    }
}

#[test]
fn cli_notes_items_whose_edits_on_both_sides_were_combined() {
    let base =
        "fn alpha() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    a + b + c\n}\n";
    let ours = base.replace("= 1", "= 10");
    let theirs = base.replace("= 3", "= 30");
    let fixture = MergeFixture::with_contents(base, &ours, &theirs);

    let (code, diagnostics) = exit(fixture.command_for("src/lib.rs").output().unwrap());

    assert_eq!(code, Some(0));
    assert_eq!(
        diagnostics,
        "git-weld: note: both sides changed fn alpha, but the edits do not overlap, \
         so they were merged line by line\n"
    );
    assert_eq!(
        fixture.result(),
        base.replace("= 1", "= 10").replace("= 3", "= 30")
    );
}

#[test]
fn clean_merges_over_size_limit_skip_semantic_analysis() {
    let padding = format!("\n// {}\n", "x".repeat(512 * 1024));
    let base = format!(
        "fn alpha() -> u32 {{\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    a + b + c\n}}\n{padding}"
    );
    let ours = base.replace("= 1", "= 10");
    let theirs = base.replace("= 3", "= 30");
    let fixture = MergeFixture::with_contents(&base, &ours, &theirs);

    let (code, diagnostics) = exit(fixture.command_for("src/lib.rs").output().unwrap());

    assert_eq!(code, Some(0));
    assert_eq!(diagnostics, "");
    assert_eq!(
        fixture.result(),
        base.replace("= 1", "= 10").replace("= 3", "= 30")
    );
}

#[test]
fn the_line_merge_matches_git_merge_file() {
    let with_use = |text: &str| format!("use std::fmt;\n\n{text}");
    let cases: Vec<(&str, [String; 3], u16)> = vec![
        (
            "lib.rs",
            [
                with_use(THREE),
                with_use(THREE)
                    .replace("use std::fmt;\n", "use std::fmt;\nuse std::io;\n")
                    .replace("    1\n", "    10\n"),
                format!("{}\nfn delta() -> u32 {{\n    4\n}}\n", with_use(THREE))
                    .replace("    1\n", "    11\n")
                    .replace("    3\n", "    33\n"),
            ],
            7,
        ),
        (
            "app.py",
            [
                "import os\n\n\ndef alpha():\n    return 1\n".to_owned(),
                "import os\nimport sys\n\n\ndef alpha():\n    return 1\n".to_owned(),
                "import os\n\n\ndef alpha():\n    return 2\n".to_owned(),
            ],
            7,
        ),
        (
            "crlf.rs",
            [
                "a\r\nb\r\nc\r\n".to_owned(),
                "a\r\nB1\r\nc\r\n".to_owned(),
                "a\r\nB2\r\nc\r\nd\r\n".to_owned(),
            ],
            7,
        ),
        (
            "unterminated.rs",
            [
                "x\nm\ny".to_owned(),
                "x\nm\ny1".to_owned(),
                "x2\nm\ny".to_owned(),
            ],
            7,
        ),
        (
            "unterminated.rs",
            ["x\ny".to_owned(), "x\ny1".to_owned(), "x\ny2".to_owned()],
            3,
        ),
        (
            "deleted.rs",
            [
                RUST_BASE.to_owned(),
                RUST_BASE
                    .replace("fn beta() -> u32 {\n    2\n}\n", "")
                    .trim_end()
                    .to_owned()
                    + "\n",
                RUST_BASE.replace("    1\n", "    100\n"),
            ],
            11,
        ),
    ];

    for (path, [base, ours, theirs], marker_size) in cases {
        let fixture = MergeFixture::with_contents(&base, &ours, &theirs);
        let expected = git_command(
            fixture.directory.path(),
            &[
                "merge-file",
                "-p",
                "--diff3",
                "-L",
                "HEAD",
                "-L",
                "base",
                "-L",
                "theirs",
                &format!("--marker-size={marker_size}"),
                "ours",
                "base",
                "theirs",
            ],
        )
        .output()
        .unwrap();

        let output = fixture
            .command_for(path)
            .args(["-l", &marker_size.to_string()])
            .output()
            .unwrap();

        assert_eq!(
            fs::read(fixture.path("ours")).unwrap(),
            expected.stdout,
            "{path}: {}",
            String::from_utf8_lossy(&expected.stdout)
        );
        assert_eq!(
            output.status.code() == Some(0),
            expected.status.code() == Some(0),
            "{path}: {output:?}"
        );
    }
}

#[test]
fn analysis_failures_still_fall_back_to_manual_resolution() {
    for (logical_path, base, ours, theirs, reason) in [
        (
            "notes.txt",
            "a\n",
            "b\n",
            "c\n",
            "unsupported file type (supported: .rs, .py)",
        ),
        (
            "app.py",
            "x = 1\n",
            "x = 2\n",
            "x = (\n",
            "theirs has a syntax error at line ",
        ),
        (
            "lib.rs",
            "fn a() {}\n",
            "fn a( {}\n",
            "fn b() {}\n",
            "ours has a syntax error at line 1",
        ),
    ] {
        let fixture = MergeFixture::with_contents(base, ours, theirs);

        let (code, diagnostics) = exit(fixture.command_for(logical_path).output().unwrap());

        assert_eq!(code, Some(1), "{logical_path}");
        assert!(
            diagnostics.contains(&format!("git-weld: semantic analysis skipped: {reason}")),
            "{diagnostics}"
        );
        assert!(
            diagnostics
                .ends_with("git-weld: 1 conflict left for manual resolution, at lines 1-7\n"),
            "{diagnostics}"
        );
        assert_eq!(fixture.result(), conflict(7, ours, base, theirs));
    }

    let fixture = MergeFixture::with_contents("fn a() {}\n", "fn a() { 1 }\n", "fn a() {}\n");
    fs::write(fixture.path("theirs"), b"fn a() { \"\xff\" }\n").unwrap();
    let (code, diagnostics) = exit(fixture.command_for("lib.rs").output().unwrap());
    assert_eq!(code, Some(1));
    assert!(diagnostics.contains("git-weld: semantic analysis skipped: theirs is not valid UTF-8"));
}

#[test]
fn accepts_paths_that_start_with_a_hyphen() {
    let directory = tempdir().unwrap();
    for (name, content) in [
        ("-base", "base\n"),
        ("-ours", "ours\n"),
        ("-theirs", "theirs\n"),
    ] {
        fs::write(directory.path().join(name), content).unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_git-weld"))
        .current_dir(directory.path())
        .env_remove("WELD_PROVIDER")
        .args(["-o", "-base", "-a", "-ours", "-b", "-theirs"])
        .args(["-p", "-source.rs"])
        .output()
        .unwrap();

    let (code, diagnostics) = exit(output);
    assert_eq!(code, Some(1));
    assert!(diagnostics.starts_with("git-weld: -source.rs: 1 conflict (provider: manual)\n"));
    assert_eq!(
        fs::read(directory.path().join("-ours")).unwrap(),
        b"<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> theirs\n"
    );
}

#[test]
fn provider_flag_overrides_environment_configuration() {
    for (environment, flag, expected) in [
        ("jev", None, "jev"),
        ("jev", Some("clef"), "clef"),
        ("clef", Some("mock"), "mock"),
    ] {
        let fixture = MergeFixture::rust_conflict(false);
        let mut command = fixture.command_for("src/lib.rs");
        command
            .env("WELD_PROVIDER", environment)
            .env("WELD_MOCK_DECISION", decision("Manual", 1.0, ""));
        if let Some(provider) = flag {
            command.args(["--provider", provider]);
        }

        let (code, diagnostics) = exit(command.output().unwrap());

        assert_eq!(code, Some(1));
        assert!(
            diagnostics.starts_with(&format!(
                "git-weld: src/lib.rs: 1 conflict in 1 chunk (provider: {expected}):\n"
            )),
            "{diagnostics}"
        );
        assert_eq!(
            diagnostics.contains("git-weld: mock decided 1 chunk:"),
            expected == "mock",
            "{diagnostics}"
        );
        assert_eq!(fixture.result(), rust_conflict_result(false));
    }
}

#[test]
fn mock_provider_resolves_confident_chunks() {
    let fixture = MergeFixture::rust_conflict(false);

    let (code, diagnostics) = exit(
        fixture
            .mock_command(&decision("TakeTheirs", 0.99, "theirs\nsupersedes ours"))
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(0));
    assert_eq!(
        diagnostics,
        "git-weld: src/lib.rs: 1 conflict in 1 chunk (provider: mock):\n\
         git-weld:   chunk 1: fn alpha\n\
         git-weld: mock decided 1 chunk:\n\
         git-weld:   chunk 1: TakeTheirs, confidence 0.99, above the 0.95 threshold: \
         theirs supersedes ours\n\
         git-weld: resolved the only chunk; no conflict markers are left\n"
    );
    assert_eq!(fixture.result(), rust_versions(false).1);
    let log = fixture.log();
    assert_eq!(log.len(), 1);
    let time = log[0]["time"].as_str().unwrap();
    assert!(time.ends_with('Z') && time.len() == 20, "{time}");
    assert_eq!(
        log[0],
        json!({
            "time": time,
            "file": "src/lib.rs",
            "chunk": 1,
            "items": ["fn alpha"],
            "lines": {"start": 1, "end": 3},
            "provider": "mock",
            "decision": "TakeTheirs",
            "confidence": 0.99,
            "reason": "theirs\nsupersedes ours",
            "resolved": true,
            "cached": false,
        })
    );
}

#[test]
fn mock_provider_leaves_low_confidence_and_manual_chunks_alone() {
    let fixture = MergeFixture::rust_conflict(true);

    let (code, diagnostics) = exit(
        fixture
            .mock_command(&format!(
                "[{}, {}]",
                decision("TakeOurs", 0.95, "ours is newer"),
                decision("Manual", 0.8, "both changes matter")
            ))
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(1));
    assert!(
        diagnostics.ends_with(
            "git-weld: mock decided 2 chunks:\n\
             git-weld:   chunk 1: TakeOurs, confidence 0.95, not above the 0.95 threshold: \
             ours is newer\n\
             git-weld:   chunk 2: Manual, confidence 0.8: both changes matter\n\
             git-weld: 2 chunks left for manual resolution, at lines 1-9 and 11-19\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(fixture.result(), rust_conflict_result(true));
    let resolved: Vec<_> = fixture
        .log()
        .iter()
        .map(|record| record["resolved"].clone())
        .collect();
    assert_eq!(resolved, [json!(false), json!(false)]);
}

#[test]
fn invalid_provider_answers_fall_back_to_manual_resolution() {
    for (answer, error) in [
        (
            decision("TakeBoth", 0.99, ""),
            "unknown variant `TakeBoth`, expected one of `TakeOurs`, `TakeTheirs`, `Manual`",
        ),
        (
            decision("TakeOurs", 1.5, ""),
            "confidence 1.5 is not between 0 and 1",
        ),
        (
            r#"{"decision": "TakeOurs", "confidence": 0.99}"#.to_owned(),
            "missing field `reason`",
        ),
        (
            format!("[{}, {{}}]", decision("TakeOurs", 0.99, "")),
            "WELD_MOCK_DECISION: decision count 2 does not match chunk count 1",
        ),
    ] {
        let fixture = MergeFixture::rust_conflict(false);

        let (code, diagnostics) = exit(fixture.mock_command(&answer).output().unwrap());

        assert_eq!(code, Some(1));
        assert!(
            diagnostics.ends_with(&format!(
                "git-weld:   chunk 1: fn alpha\n\
                 git-weld: mock evaluation failed: invalid response: {error}\n\
                 git-weld: 1 chunk left for manual resolution, at lines 1-9\n"
            )),
            "{diagnostics}"
        );
        assert_eq!(fixture.result(), rust_conflict_result(false));
        assert!(fixture.log().is_empty());
    }
}

#[test]
fn unknown_or_unconfigured_providers_fall_back_to_manual_resolution() {
    for (provider, message) in [
        (
            "clef",
            "unknown provider `clef` (available: jev, manual, mock, ollama)",
        ),
        (
            "jev",
            "provider jev is unavailable: TYPESAFE_API_KEY is not set",
        ),
        (
            "mock",
            "provider mock is unavailable: WELD_MOCK_DECISION is not set",
        ),
        (
            "ollama",
            "provider ollama is unavailable: OLLAMA_MODEL is not set",
        ),
    ] {
        let fixture = MergeFixture::rust_conflict(false);

        let output = fixture
            .command_for("src/lib.rs")
            .args(["--provider", provider])
            .output()
            .unwrap();

        let (code, diagnostics) = exit(output);
        assert_eq!(code, Some(1));
        assert!(
            diagnostics.ends_with(&format!(
                "git-weld: {message}; no model will be consulted\n\
                 git-weld: 1 chunk left for manual resolution, at lines 1-9\n"
            )),
            "{diagnostics}"
        );
        assert_eq!(fixture.result(), rust_conflict_result(false));
    }
}

#[test]
fn jev_provider_resolves_chunks_through_the_typesafe_api() {
    let server = Server::start(vec![Response::new(200, jev_answers(&[("TakeOurs", 0.98)]))]);
    let fixture = MergeFixture::rust_conflict(false);

    let (code, diagnostics) = exit(
        fixture
            .jev_command_for("src/lib.rs", &server.url())
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(0));
    assert!(
        diagnostics.ends_with(
            "git-weld: jev decided 1 chunk:\n\
             git-weld:   chunk 1: TakeOurs, confidence 0.98, above the 0.95 threshold: \
             jev-1.13.0 chose TakeOurs (probabilities: Manual 0.01, TakeOurs 0.99, TakeTheirs 0)\n\
             git-weld: resolved the only chunk; no conflict markers are left\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(fixture.result(), rust_versions(false).0);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/systemone");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer ts-test-key")
    );
    let body = requests[0].json();
    assert_eq!(body["model"], "jev-1.13.0");
    assert_eq!(body["state"]["file"], "src/lib.rs");
    assert_eq!(body["state"]["language"], "Rust");
    assert_eq!(body["state"]["chunks"][0]["items"], json!(["fn alpha"]));
    assert_eq!(
        body["state"]["chunks"][0]["theirs"]["code"],
        "fn alpha() -> u32 {\n    100\n}"
    );
}

#[test]
fn jev_asks_about_every_chunk_of_a_file_in_one_request() {
    let server = Server::start(vec![Response::new(
        200,
        jev_answers(&[("TakeOurs", 0.99), ("Manual", 0.6), ("TakeTheirs", 0.97)]),
    )]);
    let fixture = MergeFixture::three_conflicts();

    let (code, diagnostics) = exit(
        fixture
            .jev_command_for("src/lib.rs", &server.url())
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(1));
    assert!(
        diagnostics.ends_with(
            "git-weld: resolved 2 of 3 chunks; 1 left for manual resolution, at lines 5-13\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(
        fixture.result(),
        format!(
            "fn alpha() -> u32 {{\n    10\n}}\n\nfn beta() -> u32 {{\n{}}}\n\n\
             fn gamma() -> u32 {{\n    31\n}}\n",
            conflict(7, "    20\n", "    2\n", "    21\n")
        )
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body = requests[0].json();
    assert_eq!(body["state"]["chunks"].as_array().unwrap().len(), 3);
    let questions: Vec<&String> = body["questions"].as_object().unwrap().keys().collect();
    assert_eq!(questions, ["chunk_0", "chunk_1", "chunk_2"]);
}

#[test]
fn jev_failures_fall_back_to_manual_resolution() {
    let server = Server::start(vec![
        Response::new(401, r#"{"detail": "Invalid API key"}"#),
        Response::new(200, "not json"),
        Response::new(200, jev_answers(&[("TakeOurs", 0.99)])).delay(Duration::from_secs(5)),
    ]);

    for (timeout, error) in [
        (
            "10",
            r#"HTTP 401: {"detail": "Invalid API key"} (check TYPESAFE_API_KEY)"#,
        ),
        ("10", "invalid response: expected ident at line 1 column 2"),
        ("0.3", "no answer within 300ms"),
    ] {
        let fixture = MergeFixture::rust_conflict(false);
        let started = Instant::now();

        let output = fixture
            .jev_command_for("src/lib.rs", &server.url())
            .args(["--timeout", timeout])
            .output()
            .unwrap();

        assert!(started.elapsed() < Duration::from_secs(4));
        let (code, diagnostics) = exit(output);
        assert_eq!(code, Some(1));
        assert!(
            diagnostics.ends_with(&format!(
                "git-weld: jev evaluation failed: {error}\n\
                 git-weld: 1 chunk left for manual resolution, at lines 1-9\n"
            )),
            "{diagnostics}"
        );
        assert_eq!(fixture.result(), rust_conflict_result(false));
    }
}

#[test]
fn providers_are_not_consulted_for_clean_merges_or_conflicts_outside_items() {
    // With no scripted responses, any request would fail and be reported.
    let server = Server::start(Vec::new());
    let alpha = RUST_BASE.replace("    1\n", "    10\n");
    let beta = RUST_BASE.replace("    2\n", "    20\n");
    let comment = |text: &str| format!("{RUST_BASE}\n// {text}\n");

    for (path, base, ours, theirs, code) in [
        ("src/lib.rs", RUST_BASE.to_owned(), alpha, beta, 0),
        (
            "notes.txt",
            "a\n".to_owned(),
            "b\n".to_owned(),
            "c\n".to_owned(),
            1,
        ),
        (
            "src/lib.rs",
            comment("base"),
            comment("ours"),
            comment("theirs"),
            1,
        ),
    ] {
        let fixture = MergeFixture::with_contents(&base, &ours, &theirs);

        let (status, diagnostics) = exit(
            fixture
                .jev_command_for(path, &server.url())
                .output()
                .unwrap(),
        );

        assert_eq!(status, Some(code), "{diagnostics}");
        assert!(!diagnostics.contains("git-weld: jev "), "{diagnostics}");
    }
    assert!(server.requests().is_empty());
}

#[test]
fn chunks_whose_resolutions_do_not_fit_together_keep_their_markers() {
    // Each decision deletes one method; together they leave the class empty.
    let base =
        "class A:\n    def f(self):\n        return 1\n\n    def g(self):\n        return 2\n";
    let ours = "class A:\n    def f(self):\n        return 10\n\n";
    let theirs = "class A:\n\n    def g(self):\n        return 20\n";
    let fixture = MergeFixture::with_contents(base, ours, theirs);

    let (code, diagnostics) = exit(
        fixture
            .mock_command_for(
                "app.py",
                &format!(
                    "[{}, {}]",
                    decision("TakeTheirs", 0.99, "theirs deleted f"),
                    decision("TakeOurs", 0.99, "ours deleted g")
                ),
            )
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(1));
    assert!(
        diagnostics.contains(
            "git-weld: the stitched result does not parse (it has a syntax error at line "
        ),
        "{diagnostics}"
    );
    assert!(
        diagnostics.ends_with(
            "so every chunk keeps its conflict markers\n\
             git-weld: 2 chunks left for manual resolution, at lines 2-9 and 11-18\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(
        fixture.result(),
        format!(
            "class A:\n{}\n{}",
            conflict(
                7,
                "    def f(self):\n        return 10\n",
                "    def f(self):\n        return 1\n",
                ""
            ),
            conflict(
                7,
                "",
                "    def g(self):\n        return 2\n",
                "    def g(self):\n        return 20\n"
            )
        )
    );
    let resolved: Vec<_> = fixture
        .log()
        .iter()
        .map(|record| record["resolved"].clone())
        .collect();
    assert_eq!(resolved, [json!(false), json!(false)]);
}

#[test]
fn log_failures_do_not_change_the_merge_result() {
    let fixture = MergeFixture::rust_conflict(false);
    fs::create_dir(fixture.path(".git-weld.log")).unwrap();

    let (code, diagnostics) = exit(
        fixture
            .mock_command(&decision("TakeOurs", 0.99, "ours"))
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(0));
    assert!(
        diagnostics.contains("git-weld: cannot write .git-weld.log: "),
        "{diagnostics}"
    );
    assert_eq!(fixture.result(), rust_versions(false).0);
}

#[test]
fn cli_honors_git_marker_size() {
    let fixture = MergeFixture::new();
    let output = fixture
        .command()
        .args(["--marker-size", "11"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        fixture.result(),
        conflict(11, "ours\n", "base\n", "theirs\n")
    );
}

#[test]
fn input_errors_preserve_ours() {
    let fixture = MergeFixture::new();
    fs::remove_file(fixture.path("base")).unwrap();

    let output = fixture.command().output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("failed to read base file")
    );
    assert_eq!(fs::read(fixture.path("ours")).unwrap(), b"ours\n");
}

#[test]
fn invalid_arguments_preserve_ours() {
    for arguments in [
        vec!["--marker-size", "0"],
        vec!["--marker-size", "65536"],
        vec!["--marker-size", "invalid"],
        vec!["--provider", ""],
        vec!["--timeout", "0"],
        vec!["--timeout", "-1"],
        vec!["--timeout", "3601"],
        vec!["--timeout", "NaN"],
        vec!["--timeout", "soon"],
    ] {
        let fixture = MergeFixture::new();
        let output = fixture.command().args(arguments).output().unwrap();

        assert_eq!(output.status.code(), Some(2));
        assert!(!output.stderr.is_empty());
        assert_eq!(fs::read(fixture.path("ours")).unwrap(), b"ours\n");
    }
}

#[test]
fn help_and_version_exit_successfully() {
    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_git-weld"))
            .arg(argument)
            .output()
            .unwrap();

        assert!(output.status.success());
        assert!(!output.stdout.is_empty());
    }
}

fn git_command(repository: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", repository.join("no-global-config"))
        .env("GIT_CONFIG_COUNT", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_TEMPLATE_DIR")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("GIT_COMMITTER_EMAIL");
    for setting in SETTINGS {
        command.env_remove(setting);
    }
    command.args(arguments);
    command
}

fn git(repository: &Path, arguments: &[&str]) -> Output {
    git_command(repository, arguments)
        .output()
        .expect("Git must be installed to run merge-driver integration tests")
}

fn git_success(repository: &Path, arguments: &[&str]) {
    let output = git(repository, arguments);
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// The driver command for Git's configuration, in the blueprint's form:
// `WELD_PROVIDER=jev git-weld -o %O -a %A -b %B -p %P`.
fn driver(environment: &str, options: &str) -> String {
    let binary = env!("CARGO_BIN_EXE_git-weld").replace('\'', "'\\''");
    format!("{environment}'{binary}' -o %O -a %A -b %B -p %P -l %L {options}")
        .trim_end()
        .to_owned()
}

// A repository where merging branch `theirs` into `main` conflicts in `path`.
fn conflicted_repository(path: &str, [base, ours, theirs]: [&str; 3], driver: &str) -> TempDir {
    let directory = tempdir().unwrap();
    let repository = directory.path();
    git_success(
        repository,
        &["init", "--quiet", "--initial-branch=main", "--template="],
    );
    git_success(repository, &["config", "user.name", "git-weld tests"]);
    git_success(
        repository,
        &["config", "user.email", "git-weld@example.invalid"],
    );
    git_success(repository, &["config", "commit.gpgsign", "false"]);
    git_success(repository, &["config", "core.autocrlf", "false"]);
    git_success(
        repository,
        &[
            "config",
            "merge.git-weld.name",
            "git-weld Semantic Merge Driver",
        ],
    );
    git_success(repository, &["config", "merge.git-weld.driver", driver]);
    fs::write(
        repository.join(".gitattributes"),
        "*.rs merge=git-weld conflict-marker-size=11\n\
         *.py merge=git-weld conflict-marker-size=11\n",
    )
    .unwrap();
    fs::write(repository.join(path), base).unwrap();
    git_success(repository, &["add", "."]);
    git_success(repository, &["commit", "--quiet", "-m", "base"]);

    git_success(repository, &["checkout", "--quiet", "-b", "theirs"]);
    fs::write(repository.join(path), theirs).unwrap();
    git_success(repository, &["commit", "--quiet", "-am", "theirs"]);

    git_success(repository, &["checkout", "--quiet", "main"]);
    fs::write(repository.join(path), ours).unwrap();
    git_success(repository, &["commit", "--quiet", "-am", "ours"]);
    directory
}

fn assert_unmerged(repository: &Path, path: &str) {
    let unmerged = git(repository, &["ls-files", "--unmerged", "--", path]);
    assert!(unmerged.status.success());
    assert_eq!(
        String::from_utf8(unmerged.stdout).unwrap().lines().count(),
        3
    );
}

#[test]
fn git_invokes_driver_for_rust_and_python_conflicts() {
    for (path, contents, item) in [
        (
            "answer with spaces.rs",
            [
                "fn answer() -> u32 { 0 }\n",
                "fn answer() -> u32 { 1 }\n",
                "fn answer() -> u32 { 2 }\n",
            ],
            "fn answer",
        ),
        (
            "answer with spaces.py",
            [
                "def answer():\n    return 0\n",
                "def answer():\n    return 1\n",
                "def answer():\n    return 2\n",
            ],
            "def answer",
        ),
        (
            "-leading-hyphen.rs",
            [
                "fn answer() -> u32 { 0 }\n",
                "fn answer() -> u32 { 1 }\n",
                "fn answer() -> u32 { 2 }\n",
            ],
            "fn answer",
        ),
    ] {
        let directory = conflicted_repository(path, contents, &driver("", "--provider manual"));
        let repository = directory.path();

        let output = git(repository, &["merge", "--no-edit", "theirs"]);

        assert_eq!(output.status.code(), Some(1));
        let diagnostics = String::from_utf8(output.stderr).unwrap();
        assert!(
            diagnostics.contains(&format!(
                "git-weld: {path}: 1 conflict in 1 chunk (provider: manual):\n\
                 git-weld:   chunk 1: {item}\n"
            )),
            "{diagnostics}"
        );
        let [base, ours, theirs] = contents;
        let lines = |text: &str| {
            text.strip_prefix(if path.ends_with(".py") {
                "def answer():\n"
            } else {
                ""
            })
            .unwrap()
            .to_owned()
        };
        let expected = if path.ends_with(".py") {
            format!(
                "def answer():\n{}",
                conflict(11, &lines(ours), &lines(base), &lines(theirs))
            )
        } else {
            conflict(11, ours, base, theirs)
        };
        assert_eq!(fs::read_to_string(repository.join(path)).unwrap(), expected);
        assert_unmerged(repository, path);
    }
}

#[test]
fn git_merge_combines_edits_that_do_not_overlap_without_a_model() {
    let ours = RUST_BASE.replace("    1\n", "    10\n");
    let theirs = RUST_BASE.replace("    2\n", "    20\n");
    // With no scripted responses, any request would fail and be reported.
    let server = Server::start(Vec::new());
    let directory = conflicted_repository(
        "lib.rs",
        [RUST_BASE, &ours, &theirs],
        &driver("", "--provider jev"),
    );
    let repository = directory.path();

    let output = git_command(repository, &["merge", "--no-edit", "theirs"])
        .env("TYPESAFE_API_KEY", "ts-test-key")
        .env("TYPESAFE_BASE_URL", server.url())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("git-weld")
    );
    assert_eq!(
        fs::read_to_string(repository.join("lib.rs")).unwrap(),
        RUST_BASE
            .replace("    1\n", "    10\n")
            .replace("    2\n", "    20\n")
    );
    git_success(repository, &["rev-parse", "--verify", "--quiet", "HEAD^2"]);
    assert!(server.requests().is_empty());
}

#[test]
fn git_merge_completes_when_every_chunk_is_resolved() {
    let contents = [
        "def answer():\n    return 0\n",
        "def answer():\n    return 1\n",
        "def answer():\n    return 2\n",
    ];
    let directory =
        conflicted_repository("answer.py", contents, &driver("WELD_PROVIDER=mock ", ""));
    let repository = directory.path();

    let output = git_command(repository, &["merge", "--no-edit", "theirs"])
        .env(
            "WELD_MOCK_DECISION",
            decision("TakeTheirs", 0.99, "theirs is newer"),
        )
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostics.contains(
            "git-weld: mock decided 1 chunk:\n\
             git-weld:   chunk 1: TakeTheirs, confidence 0.99, above the 0.95 threshold: \
             theirs is newer\n\
             git-weld: resolved the only chunk; no conflict markers are left\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(
        fs::read_to_string(repository.join("answer.py")).unwrap(),
        contents[2]
    );
    let unmerged = git(repository, &["ls-files", "--unmerged"]);
    assert!(unmerged.stdout.is_empty());
    git_success(repository, &["rev-parse", "--verify", "--quiet", "HEAD^2"]);
}

#[test]
fn git_merge_leaves_markers_only_around_the_chunk_with_a_manual_decision() {
    let base = format!("use std::fmt;\n\n{THREE}\nfn untouched() {{}}\n");
    let ours = base
        .replace("use std::fmt;\n", "use std::fmt;\nuse std::io;\n")
        .replace("    1\n", "    10\n")
        .replace("    2\n", "    20\n")
        .replace("    3\n", "    30\n");
    let theirs = format!("{base}\nfn delta() -> u32 {{\n    4\n}}\n")
        .replace("    1\n", "    11\n")
        .replace("    2\n", "    21\n")
        .replace("    3\n", "    31\n");
    let directory = conflicted_repository(
        "lib.rs",
        [&base, &ours, &theirs],
        &driver("", "--provider mock"),
    );
    let repository = directory.path();

    let output = git_command(repository, &["merge", "--no-edit", "theirs"])
        .env(
            "WELD_MOCK_DECISION",
            format!(
                "[{}, {}, {}]",
                decision("TakeOurs", 0.99, "ours includes theirs"),
                decision("Manual", 0.7, "both changes matter"),
                decision("TakeTheirs", 0.99, "theirs includes ours")
            ),
        )
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostics.contains(
            "git-weld: resolved 2 of 3 chunks; 1 left for manual resolution, at lines 8-16\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(
        fs::read_to_string(repository.join("lib.rs")).unwrap(),
        format!(
            "use std::fmt;\nuse std::io;\n\nfn alpha() -> u32 {{\n    10\n}}\n\n\
             fn beta() -> u32 {{\n{}}}\n\nfn gamma() -> u32 {{\n    31\n}}\n\n\
             fn untouched() {{}}\n\nfn delta() -> u32 {{\n    4\n}}\n",
            conflict(11, "    20\n", "    2\n", "    21\n")
        )
    );
    assert_unmerged(repository, "lib.rs");

    let log = read_log(repository);
    let summary: Vec<_> = log
        .iter()
        .map(|record| {
            (
                record["chunk"].clone(),
                record["items"].clone(),
                record["lines"].clone(),
                record["resolved"].clone(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                json!(1),
                json!(["fn alpha"]),
                json!({"start": 4, "end": 6}),
                json!(true)
            ),
            (
                json!(2),
                json!(["fn beta"]),
                json!({"start": 8, "end": 16}),
                json!(false)
            ),
            (
                json!(3),
                json!(["fn gamma"]),
                json!({"start": 18, "end": 20}),
                json!(true)
            ),
        ]
    );
}

#[test]
fn ollama_provider_evaluates_systemone_models_without_an_api_key() {
    let server = Server::start(vec![Response::new(
        200,
        json!({
            "model": "kev:0.8b-bf16",
            "answers": {
                "chunk_0": {
                    "type": "choice",
                    "choice": "TakeOurs",
                    "confidence": 0.99,
                    "probabilities": {"Manual": 0.0, "TakeOurs": 0.99, "TakeTheirs": 0.01},
                },
            },
            "usage": {"input_tokens": 120, "output_tokens": 0},
        })
        .to_string(),
    )]);
    let fixture = MergeFixture::rust_conflict(false);

    let (code, diagnostics) = exit(
        fixture
            .command_for("src/lib.rs")
            .args(["--provider", "ollama"])
            .env("OLLAMA_HOST", server.url())
            .env("OLLAMA_MODEL", "kev:0.8b-bf16")
            .output()
            .unwrap(),
    );

    assert_eq!(code, Some(0), "{diagnostics}");
    assert!(
        diagnostics.contains("git-weld: ollama decided 1 chunk:\n"),
        "{diagnostics}"
    );
    assert_eq!(fixture.result(), rust_versions(false).0);
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/v1/systemone");
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(requests[0].json()["model"], "kev:0.8b-bf16");
}

#[test]
fn trims_hoisted_signatures_to_fit_the_request_limit_and_does_not_cache_trimmed_answers() {
    let server = Server::start(vec![
        Response::new(200, jev_answers(&[("TakeOurs", 0.99)])),
        Response::new(200, jev_answers(&[("TakeOurs", 0.99)])),
    ]);
    let huge_comment = format!("/// {}\n", "x".repeat(128 * 1024));
    let base = format!(
        "struct Small {{\n    id: u32,\n}}\n\n\
         {huge_comment}struct Huge {{\n    id: u32,\n}}\n\n\
         fn alpha(small: &Small, huge: &Huge) -> u32 {{\n    small.id + huge.id + 1\n}}\n"
    );
    let ours = base.replace("small.id + huge.id + 1", "small.id + huge.id + 10");
    let theirs = base.replace("small.id + huge.id + 1", "small.id + huge.id + 100");
    let fixture = MergeFixture::with_contents(&base, &ours, &theirs);
    git_success(
        fixture.directory.path(),
        &["init", "--quiet", "--initial-branch=main", "--template="],
    );

    for _ in 0..2 {
        fs::write(fixture.path("ours"), &ours).unwrap();
        let (code, diagnostics) = exit(
            fixture
                .jev_command_for("src/lib.rs", &server.url())
                .output()
                .unwrap(),
        );
        assert_eq!(code, Some(0), "{diagnostics}");
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let sent_state = &requests[0].json()["state"];
    assert_eq!(sent_state["chunks"].as_array().unwrap().len(), 1);
    assert_eq!(
        sent_state["signatures"],
        json!([{"item": "struct Small", "signature": "struct Small {\n    id: u32,\n}"}])
    );
}

#[test]
fn decision_cache_answers_hits_sends_only_misses_and_honors_no_cache() {
    let server = Server::start(vec![
        Response::new(200, jev_answers(&[("TakeOurs", 0.99), ("Manual", 0.7)])),
        Response::new(200, jev_answers(&[("TakeTheirs", 0.98)])),
        Response::new(
            200,
            jev_answers(&[("TakeOurs", 0.99), ("Manual", 0.7), ("TakeTheirs", 0.98)]),
        ),
        Response::new(400, "bad request"),
    ]);
    let fixture = MergeFixture::rust_conflict(true);
    git_success(
        fixture.directory.path(),
        &["init", "--quiet", "--initial-branch=main", "--template="],
    );

    // Run 1: both chunks miss and are stored in `.git/git-weld/cache`.
    let (code, diagnostics) = exit(
        fixture
            .jev_command_for("src/lib.rs", &server.url())
            .output()
            .unwrap(),
    );
    assert_eq!(code, Some(1), "{diagnostics}");
    assert_eq!(server.requests().len(), 1);
    assert!(
        fixture
            .directory
            .path()
            .join(".git/git-weld/cache")
            .is_file()
    );
    let cached_flags: Vec<_> = fixture
        .log()
        .iter()
        .map(|record| record["cached"].clone())
        .collect();
    assert_eq!(cached_flags, [json!(false), json!(false)]);

    // Run 2: add a third conflicting function; chunks 1 and 2 hit the cache,
    // and only chunk 3 is sent to Jev. An empty `WELD_NO_CACHE=""` is treated
    // as unset so the cache stays active.
    let edit = |suffix: &str| {
        THREE
            .replace("    1\n", &format!("    1{suffix}\n"))
            .replace("    2\n", &format!("    2{suffix}\n"))
            .replace("    3\n", &format!("    3{suffix}\n"))
    };
    fs::write(fixture.path("base"), THREE).unwrap();
    fs::write(fixture.path("ours"), edit("0")).unwrap();
    fs::write(fixture.path("theirs"), edit("00")).unwrap();

    let (code, diagnostics) = exit(
        fixture
            .jev_command_for("src/lib.rs", &server.url())
            .env("WELD_NO_CACHE", "")
            .output()
            .unwrap(),
    );
    assert_eq!(code, Some(1), "{diagnostics}");
    assert!(
        diagnostics.contains("git-weld:   chunk 1: TakeOurs (cached), confidence 0.99"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("git-weld:   chunk 2: Manual (cached), confidence 0.7"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("git-weld:   chunk 3: TakeTheirs, confidence 0.98"),
        "{diagnostics}"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].json()["state"]["chunks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        requests[1].json()["state"]["chunks"][0]["items"],
        json!(["fn gamma"])
    );
    let log = fixture.log();
    let latest_cached: Vec<_> = log[2..]
        .iter()
        .map(|record| record["cached"].clone())
        .collect();
    assert_eq!(latest_cached, [json!(true), json!(true), json!(false)]);

    // Run 3: `--no-cache` bypasses the cache and asks Jev for all 3 chunks.
    fs::write(fixture.path("ours"), edit("0")).unwrap();
    let (code, diagnostics) = exit(
        fixture
            .jev_command_for("src/lib.rs", &server.url())
            .arg("--no-cache")
            .output()
            .unwrap(),
    );
    assert_eq!(code, Some(1), "{diagnostics}");
    assert!(!diagnostics.contains("(cached)"), "{diagnostics}");
    assert_eq!(server.requests().len(), 3);

    // Run 4: if a new chunk misses and the provider fails (500), cached hits
    // for the other chunks still resolve their chunks.
    let four_base = format!("{THREE}\nfn delta() -> u32 {{\n    4\n}}\n");
    let four_ours = format!("{}\nfn delta() -> u32 {{\n    40\n}}\n", edit("0"));
    let four_theirs = format!("{}\nfn delta() -> u32 {{\n    400\n}}\n", edit("00"));
    fs::write(fixture.path("base"), &four_base).unwrap();
    fs::write(fixture.path("ours"), &four_ours).unwrap();
    fs::write(fixture.path("theirs"), &four_theirs).unwrap();

    let (code, diagnostics) = exit(
        fixture
            .jev_command_for("src/lib.rs", &server.url())
            .args(["--timeout", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(code, Some(1), "{diagnostics}");
    assert!(
        diagnostics.contains("git-weld: jev evaluation failed: HTTP 400: bad request"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains("git-weld: resolved 2 of 4 chunks; 2 left for manual resolution"),
        "{diagnostics}"
    );
}

#[test]
fn git_rebase_reuses_cached_decisions_when_replaying_the_same_conflict() {
    let server = Server::start(vec![Response::new(
        200,
        jev_answers(&[("TakeTheirs", 0.99)]),
    )]);
    let base = "\
struct User {
    x: u32,
}

fn check(user: &User) -> i32 {
    user.x as i32
}
";
    let ours = base
        .replace("x: u32,", "x: i32,")
        .replace("user.x as i32", "user.x + 1");
    let theirs = base.replace("user.x as i32", "user.x as i32 + 2");
    let driver_cmd = format!(
        "TYPESAFE_API_KEY=ts-test-key TYPESAFE_BASE_URL={} {}",
        server.url(),
        driver("", "--provider jev")
    );
    let directory = conflicted_repository("lib.rs", [base, &ours, &theirs], &driver_cmd);
    let repository = directory.path();

    // First rebase of `theirs` onto `main`: Jev is asked once (with `struct User`
    // hoisted into the state) and resolves the conflict.
    let first = git(repository, &["rebase", "main", "theirs"]);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let first_resolved = fs::read_to_string(repository.join("lib.rs")).unwrap();
    assert_eq!(
        first_resolved,
        "struct User {\n    x: i32,\n}\n\nfn check(user: &User) -> i32 {\n    user.x as i32 + 2\n}\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].json()["state"]["signatures"],
        json!([{
            "item": "struct User",
            "base": "struct User {\n    x: u32,\n}",
            "ours": "struct User {\n    x: i32,\n}",
            "theirs": "struct User {\n    x: u32,\n}",
        }])
    );

    // Reset `theirs` back to its pre-rebase commit and rebase onto `main`
    // again: the conflict is answered from `.git/git-weld/cache` without a
    // second request to the server.
    git_success(repository, &[" reset".trim(), "--hard", "ORIG_HEAD"]);
    let second = git(repository, &["rebase", "main", "theirs"]);
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    let second_diagnostics = String::from_utf8(second.stderr).unwrap();
    assert!(
        second_diagnostics.contains("TakeTheirs (cached), confidence 0.99"),
        "{second_diagnostics}"
    );
    assert_eq!(
        fs::read_to_string(repository.join("lib.rs")).unwrap(),
        first_resolved
    );
    assert_eq!(server.requests().len(), 1);
    let log = read_log(repository);
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["cached"], json!(false));
    assert_eq!(log[1]["cached"], json!(true));
}

#[test]
fn git_worktree_shares_the_decision_cache_across_worktrees() {
    let server = Server::start(vec![Response::new(200, jev_answers(&[("TakeOurs", 0.99)]))]);
    let (ours, theirs) = rust_versions(false);
    let driver_cmd = format!(
        "TYPESAFE_API_KEY=ts-test-key TYPESAFE_BASE_URL={} {}",
        server.url(),
        driver("", "--provider jev")
    );
    let directory = conflicted_repository("lib.rs", [RUST_BASE, &ours, &theirs], &driver_cmd);
    let repository = directory.path();

    // First merge in the main worktree populates `<git-common-dir>/git-weld/cache`.
    let first = git(repository, &["merge", "--no-edit", "theirs"]);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    assert_eq!(fs::read_to_string(repository.join("lib.rs")).unwrap(), ours);
    assert_eq!(server.requests().len(), 1);
    assert!(repository.join(".git/git-weld/cache").is_file());

    // Create a linked worktree at the pre-merge `ours` commit (`HEAD^1`) and
    // merge `theirs` there: the linked worktree resolves the conflict from the
    // shared cache without a second request to the provider.
    let worktree_dir = tempdir().unwrap();
    let linked = worktree_dir.path().join("linked");
    git_success(
        repository,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked-ours",
            linked.to_str().unwrap(),
            "HEAD^1",
        ],
    );
    assert!(linked.join(".git").is_file());

    let second = git(&linked, &["merge", "--no-edit", "theirs"]);
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    let second_diagnostics = String::from_utf8(second.stderr).unwrap();
    assert!(
        second_diagnostics.contains("TakeOurs (cached), confidence 0.99"),
        "{second_diagnostics}"
    );
    assert_eq!(fs::read_to_string(linked.join("lib.rs")).unwrap(), ours);
    assert_eq!(server.requests().len(), 1);
    let log = read_log(&linked);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["cached"], json!(true));
    assert_eq!(log[0]["resolved"], json!(true));
}
