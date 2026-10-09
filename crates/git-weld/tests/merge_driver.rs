mod support;

use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};
use support::{Response, Server};
use tempfile::{TempDir, tempdir};

// Settings that would let the caller's environment leak into a test.
const SETTINGS: [&str; 6] = [
    "WELD_PROVIDER",
    "WELD_TIMEOUT",
    "WELD_MOCK_DECISION",
    "TYPESAFE_API_KEY",
    "TYPESAFE_BASE_URL",
    "TYPESAFE_DEFAULT_MODEL",
];

const RUST_BASE: &str = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";
const ALPHA_CONFLICT: &str = "git-weld:   fn alpha: modified on both sides \
                              (base lines 1-3, ours lines 1-3, theirs lines 1-3)\n";

struct MergeFixture {
    directory: TempDir,
    expected_conflict: String,
}

impl MergeFixture {
    fn new() -> Self {
        Self::with_contents("base\n", "ours\n", "theirs\n")
    }

    fn with_contents(base: &str, ours: &str, theirs: &str) -> Self {
        let fixture = Self {
            directory: tempdir().unwrap(),
            expected_conflict: format!(
                "<<<<<<< HEAD\n{ours}||||||| base\n{base}=======\n{theirs}>>>>>>> theirs\n"
            ),
        };
        fs::write(fixture.path("base"), base).unwrap();
        fs::write(fixture.path("ours"), ours).unwrap();
        fs::write(fixture.path("theirs"), theirs).unwrap();
        fixture
    }

    // `fn alpha` conflicts; with `both`, `fn beta` does too.
    fn rust_conflict(both: bool) -> Self {
        let mut ours = RUST_BASE.replace("    1\n", "    10\n");
        let mut theirs = RUST_BASE.replace("    1\n", "    100\n");
        if both {
            ours = ours.replace("    2\n", "    20\n");
            theirs = theirs.replace("    2\n", "    200\n");
        }
        Self::with_contents(RUST_BASE, &ours, &theirs)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn command(&self) -> Command {
        self.command_for("source file.rs")
    }

    fn command_for(&self, logical_path: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-weld"));
        for setting in SETTINGS {
            command.env_remove(setting);
        }
        command
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
        let mut command = self.command_for("src/lib.rs");
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

    fn assert_manual_conflict(&self) {
        assert_eq!(
            fs::read_to_string(self.path("ours")).unwrap(),
            self.expected_conflict
        );
    }
}

fn diagnostics(output: Output) -> String {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    String::from_utf8(output.stderr).unwrap()
}

fn jev_answer(choice: &str, confidence: f64) -> String {
    json!({
        "model": "jev-1.13.0",
        "answers": {
            "conflict_0": {
                "type": "choice",
                "choice": choice,
                "confidence": confidence,
                "probabilities": {"Manual": 0.01, "TakeOurs": 0.99, "TakeTheirs": 0.0},
            },
        },
        "usage": {"input_tokens": 512, "output_tokens": 34},
    })
    .to_string()
}

#[test]
fn cli_writes_conflicts_and_reports_input_paths() {
    let fixture = MergeFixture::new();
    let output = fixture.command().output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostics.contains("manual merge required for source file.rs (provider: manual)"));
    for name in ["base", "ours", "theirs"] {
        assert!(diagnostics.contains(&fixture.path(name).display().to_string()));
    }
    assert_eq!(
        fs::read(fixture.path("ours")).unwrap(),
        b"<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> theirs\n"
    );
}

#[test]
fn cli_reports_items_changed_on_both_sides() {
    let base = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";
    let ours = base
        .replace("    1\n", "    10\n")
        .replace("    2\n", "    20\n");
    let theirs = base
        .replace("    1\n", "    100\n")
        .replace("    2\n", "    200\n");
    let fixture = MergeFixture::with_contents(base, &ours, &theirs);

    let output = fixture.command_for("src/lib.rs").output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostics.contains("manual merge required for src/lib.rs"));
    assert!(
        diagnostics.ends_with(
            "git-weld: 2 Rust items changed on both sides:\n\
             git-weld:   fn alpha: modified on both sides \
             (base lines 1-3, ours lines 1-3, theirs lines 1-3)\n\
             git-weld:   fn beta: modified on both sides \
             (base lines 5-7, ours lines 5-7, theirs lines 5-7)\n"
        ),
        "{diagnostics}"
    );
    assert_eq!(
        fs::read_to_string(fixture.path("ours")).unwrap(),
        format!("<<<<<<< HEAD\n{ours}||||||| base\n{base}=======\n{theirs}>>>>>>> theirs\n")
    );
}

#[test]
fn cli_reports_when_edits_touch_different_items() {
    let base = "def alpha():\n    return 1\n\n\ndef beta():\n    return 2\n";
    let ours = base.replace("return 1", "return 10");
    let theirs = base.replace("return 2", "return 20");
    let fixture = MergeFixture::with_contents(base, &ours, &theirs);

    let output = fixture.command_for("app.py").output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .ends_with("git-weld: no Python item changed on both sides\n")
    );
    assert!(
        fs::read_to_string(fixture.path("ours"))
            .unwrap()
            .starts_with("<<<<<<< HEAD\n")
    );
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

        let output = fixture.command_for(logical_path).output().unwrap();

        assert_eq!(output.status.code(), Some(1), "{logical_path}");
        let diagnostics = String::from_utf8(output.stderr).unwrap();
        assert!(
            diagnostics.contains(&format!("git-weld: semantic analysis skipped: {reason}")),
            "{diagnostics}"
        );
        assert_eq!(
            fs::read_to_string(fixture.path("ours")).unwrap(),
            format!("<<<<<<< HEAD\n{ours}||||||| base\n{base}=======\n{theirs}>>>>>>> theirs\n")
        );
    }

    let fixture = MergeFixture::with_contents("fn a() {}\n", "fn a() {}\n", "fn a() {}\n");
    fs::write(fixture.path("theirs"), b"fn a() { \"\xff\" }\n").unwrap();
    let output = fixture.command_for("lib.rs").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("git-weld: semantic analysis skipped: theirs is not valid UTF-8")
    );
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

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("manual merge required for -source.rs")
    );
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
        command.env("WELD_PROVIDER", environment).env(
            "WELD_MOCK_DECISION",
            r#"{"decision": "Manual", "confidence": 1, "reason": ""}"#,
        );
        if let Some(provider) = flag {
            command.args(["--provider", provider]);
        }

        let diagnostics = diagnostics(command.output().unwrap());

        assert!(
            diagnostics.contains(&format!(
                "manual merge required for src/lib.rs (provider: {expected})"
            )),
            "{diagnostics}"
        );
        assert_eq!(
            diagnostics.contains("git-weld: mock decided 1 item:"),
            expected == "mock",
            "{diagnostics}"
        );
        fixture.assert_manual_conflict();
    }
}

#[test]
fn mock_provider_reports_confident_decisions_without_resolving_yet() {
    let fixture = MergeFixture::rust_conflict(false);

    let output = fixture
        .mock_command(
            r#"{"decision": "TakeTheirs", "confidence": 0.99, "reason": "theirs\nsupersedes ours"}"#,
        )
        .output()
        .unwrap();

    let diagnostics = diagnostics(output);
    assert!(
        diagnostics.ends_with(&format!(
            "{ALPHA_CONFLICT}\
             git-weld: mock decided 1 item:\n\
             git-weld:   fn alpha: TakeTheirs, confidence 0.99, above the 0.95 threshold: \
             theirs supersedes ours\n\
             git-weld: automatic resolution is not implemented yet; \
             the conflict is left for manual resolution\n"
        )),
        "{diagnostics}"
    );
    fixture.assert_manual_conflict();
}

#[test]
fn mock_provider_reports_low_confidence_and_manual_decisions() {
    let fixture = MergeFixture::rust_conflict(true);

    let output = fixture
        .mock_command(
            r#"[
                {"decision": "TakeOurs", "confidence": 0.95, "reason": "ours is newer"},
                {"decision": "Manual", "confidence": 0.8, "reason": "both changes matter"}
            ]"#,
        )
        .output()
        .unwrap();

    let diagnostics = diagnostics(output);
    assert!(
        diagnostics.ends_with(
            "git-weld: mock decided 2 items:\n\
             git-weld:   fn alpha: TakeOurs, confidence 0.95, not above the 0.95 threshold: \
             ours is newer\n\
             git-weld:   fn beta: Manual, confidence 0.8: both changes matter\n"
        ),
        "{diagnostics}"
    );
    fixture.assert_manual_conflict();
}

#[test]
fn invalid_provider_answers_fall_back_to_manual_resolution() {
    for (answer, error) in [
        (
            r#"{"decision": "TakeBoth", "confidence": 0.99, "reason": ""}"#,
            "unknown variant `TakeBoth`, expected one of `TakeOurs`, `TakeTheirs`, `Manual`",
        ),
        (
            r#"{"decision": "TakeOurs", "confidence": 1.5, "reason": ""}"#,
            "confidence 1.5 is not between 0 and 1",
        ),
        (
            r#"{"decision": "TakeOurs", "confidence": 0.99}"#,
            "missing field `reason`",
        ),
        (
            r#"[{"decision": "TakeOurs", "confidence": 0.99, "reason": ""}, {}]"#,
            "WELD_MOCK_DECISION: decision count 2 does not match conflicting item count 1",
        ),
    ] {
        let fixture = MergeFixture::rust_conflict(false);

        let diagnostics = diagnostics(fixture.mock_command(answer).output().unwrap());

        assert!(
            diagnostics.ends_with(&format!(
                "{ALPHA_CONFLICT}git-weld: mock evaluation failed: invalid response: {error}\n"
            )),
            "{diagnostics}"
        );
        fixture.assert_manual_conflict();
    }
}

#[test]
fn unknown_or_unconfigured_providers_fall_back_to_manual_resolution() {
    for (provider, message) in [
        (
            "clef",
            "unknown provider `clef` (available: jev, manual, mock)",
        ),
        (
            "jev",
            "provider jev is unavailable: TYPESAFE_API_KEY is not set",
        ),
        (
            "mock",
            "provider mock is unavailable: WELD_MOCK_DECISION is not set",
        ),
    ] {
        let fixture = MergeFixture::rust_conflict(false);

        let output = fixture
            .command_for("src/lib.rs")
            .args(["--provider", provider])
            .output()
            .unwrap();

        let diagnostics = diagnostics(output);
        assert!(
            diagnostics.contains(&format!(
                "git-weld: {message}; no model will be consulted\n"
            )),
            "{diagnostics}"
        );
        assert!(diagnostics.ends_with(ALPHA_CONFLICT), "{diagnostics}");
        fixture.assert_manual_conflict();
    }
}

#[test]
fn jev_provider_asks_the_typesafe_api() {
    let server = Server::start(vec![Response::new(200, jev_answer("TakeOurs", 0.98))]);
    let fixture = MergeFixture::rust_conflict(false);

    let output = fixture
        .jev_command_for("src/lib.rs", &server.url())
        .output()
        .unwrap();

    let diagnostics = diagnostics(output);
    assert!(
        diagnostics.ends_with(&format!(
            "{ALPHA_CONFLICT}\
             git-weld: jev decided 1 item:\n\
             git-weld:   fn alpha: TakeOurs, confidence 0.98, above the 0.95 threshold: \
             jev-1.13.0 chose TakeOurs (probabilities: Manual 0.01, TakeOurs 0.99, TakeTheirs 0)\n\
             git-weld: automatic resolution is not implemented yet; \
             the conflict is left for manual resolution\n"
        )),
        "{diagnostics}"
    );
    fixture.assert_manual_conflict();

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
    assert_eq!(body["state"]["conflicts"][0]["item"], "fn alpha");
    assert_eq!(
        body["state"]["conflicts"][0]["theirs"]["code"],
        "fn alpha() -> u32 {\n    100\n}"
    );
}

#[test]
fn jev_failures_fall_back_to_manual_resolution() {
    let server = Server::start(vec![
        Response::new(401, r#"{"detail": "Invalid API key"}"#),
        Response::new(200, "not json"),
        Response::new(200, jev_answer("TakeOurs", 0.99)).delay(Duration::from_secs(5)),
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
        let diagnostics = diagnostics(output);
        assert!(
            diagnostics.ends_with(&format!(
                "{ALPHA_CONFLICT}git-weld: jev evaluation failed: {error}\n"
            )),
            "{diagnostics}"
        );
        fixture.assert_manual_conflict();
    }
}

#[test]
fn providers_are_not_consulted_without_items_changed_on_both_sides() {
    // With no scripted responses, any request would fail and be reported.
    let server = Server::start(Vec::new());
    let alpha = RUST_BASE.replace("    1\n", "    10\n");
    let beta = RUST_BASE.replace("    2\n", "    20\n");

    for (path, base, ours, theirs) in [
        ("src/lib.rs", RUST_BASE, alpha.as_str(), beta.as_str()),
        ("notes.txt", "a\n", "b\n", "c\n"),
    ] {
        let fixture = MergeFixture::with_contents(base, ours, theirs);

        let output = fixture
            .jev_command_for(path, &server.url())
            .output()
            .unwrap();

        let diagnostics = diagnostics(output);
        assert!(!diagnostics.contains("git-weld: jev "), "{diagnostics}");
        fixture.assert_manual_conflict();
    }
    assert!(server.requests().is_empty());
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
        fs::read(fixture.path("ours")).unwrap(),
        b"<<<<<<<<<<< HEAD\nours\n||||||||||| base\nbase\n===========\ntheirs\n>>>>>>>>>>> theirs\n"
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

fn assert_left_unmerged(repository: &Path, path: &str, [base, ours, theirs]: [&str; 3]) {
    assert_eq!(
        fs::read_to_string(repository.join(path)).unwrap(),
        format!(
            "<<<<<<<<<<< HEAD\n{ours}||||||||||| base\n{base}\
             ===========\n{theirs}>>>>>>>>>>> theirs\n"
        )
    );
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
        assert!(diagnostics.contains(&format!("manual merge required for {path}")));
        assert!(
            diagnostics.contains(&format!(
                "git-weld:   {item}: modified on both sides (base line"
            )),
            "{diagnostics}"
        );
        assert_left_unmerged(repository, path, contents);
    }
}

#[test]
fn git_merge_passes_the_provider_configuration_to_the_driver() {
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
            r#"{"decision": "TakeTheirs", "confidence": 0.99, "reason": "theirs is newer"}"#,
        )
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(
        diagnostics.contains("git-weld: manual merge required for answer.py (provider: mock)\n"),
        "{diagnostics}"
    );
    assert!(
        diagnostics.contains(
            "git-weld: mock decided 1 item:\n\
             git-weld:   def answer: TakeTheirs, confidence 0.99, above the 0.95 threshold: \
             theirs is newer\n"
        ),
        "{diagnostics}"
    );
    assert_left_unmerged(repository, "answer.py", contents);
}
