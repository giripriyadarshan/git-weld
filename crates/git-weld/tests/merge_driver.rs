use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::{TempDir, tempdir};

struct MergeFixture {
    directory: TempDir,
}

impl MergeFixture {
    fn new() -> Self {
        let fixture = Self {
            directory: tempdir().unwrap(),
        };
        fs::write(fixture.path("base"), "base\n").unwrap();
        fs::write(fixture.path("ours"), "ours\n").unwrap();
        fs::write(fixture.path("theirs"), "theirs\n").unwrap();
        fixture
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-weld"));
        command
            .env_remove("WELD_PROVIDER")
            .arg("-o")
            .arg(self.path("base"))
            .arg("-a")
            .arg(self.path("ours"))
            .arg("-b")
            .arg(self.path("theirs"))
            .args(["-p", "source file.rs"]);
        command
    }
}

#[test]
fn cli_writes_conflicts_and_reports_input_paths() {
    let fixture = MergeFixture::new();
    let output = fixture.command().output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let diagnostics = String::from_utf8(output.stderr).unwrap();
    assert!(diagnostics.contains("manual merge required for source file.rs"));
    assert!(diagnostics.contains("requested provider: manual"));
    for name in ["base", "ours", "theirs"] {
        assert!(diagnostics.contains(&fixture.path(name).display().to_string()));
    }
    assert_eq!(
        fs::read(fixture.path("ours")).unwrap(),
        b"<<<<<<< HEAD\nours\n||||||| base\nbase\n=======\ntheirs\n>>>>>>> theirs\n"
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
    for (flag, expected_provider) in [(None, "jev"), (Some("clef"), "clef")] {
        let fixture = MergeFixture::new();
        let mut command = fixture.command();
        command.env("WELD_PROVIDER", "jev");
        if let Some(provider) = flag {
            command.args(["--provider", provider]);
        }
        let output = command.output().unwrap();

        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains(&format!("requested provider: {expected_provider}"))
        );
    }
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

fn git(repository: &Path, arguments: &[&str]) -> Output {
    Command::new("git")
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
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("WELD_PROVIDER")
        .args(arguments)
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

#[test]
fn git_invokes_driver_for_rust_and_python_conflicts() {
    for (path, base, ours, theirs) in [
        (
            "answer with spaces.rs",
            "fn answer() -> u32 { 0 }\n",
            "fn answer() -> u32 { 1 }\n",
            "fn answer() -> u32 { 2 }\n",
        ),
        (
            "answer with spaces.py",
            "def answer():\n    return 0\n",
            "def answer():\n    return 1\n",
            "def answer():\n    return 2\n",
        ),
        (
            "-leading-hyphen.rs",
            "fn answer() -> u32 { 0 }\n",
            "fn answer() -> u32 { 1 }\n",
            "fn answer() -> u32 { 2 }\n",
        ),
    ] {
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
        let binary = env!("CARGO_BIN_EXE_git-weld").replace('\'', "'\\''");
        let driver = format!("'{binary}' -o %O -a %A -b %B -p %P -l %L --provider manual");
        git_success(repository, &["config", "merge.git-weld.driver", &driver]);
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

        let output = git(repository, &["merge", "--no-edit", "theirs"]);

        assert_eq!(output.status.code(), Some(1));
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains(&format!("manual merge required for {path}"))
        );
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
}
