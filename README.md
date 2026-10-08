# git-weld

A model-agnostic semantic Git merge driver in Rust, following
[project_blueprint.md](project_blueprint.md).

## Current scope

This initializes **Phase 1: Foundation & Git Hook Plumbing**. The Rust 2024
workspace contains a `clap` CLI, a reusable merge module, and real Git integration
tests for Rust and Python files.

The driver currently **always requires manual resolution**. It reads the base,
ours, and theirs files, writes a full-file diff3 conflict into `%A`, reports the
input paths on stderr, and exits with code `1`. All three versions are included;
missing final newlines are added so markers stay on separate lines. Replacement
is atomic and preserves the output file's permissions. Input or output errors
are reported on stderr and exit with code `2`.

No AST parsing, remote model calls, automatic resolutions, or `.git-weld.log`
generation are implemented yet. `--provider` and `WELD_PROVIDER` capture the
requested provider only; choosing `jev`, `clef`, or `sap-rst` does not enable a
backend. A CLI flag takes precedence over the environment, and the default is
`manual`. Even changes that a normal text merge could combine are left for
manual resolution in this foundation build.

## Build and run

Install a stable Rust toolchain (1.85 or newer, required by edition 2024) and
Git, then run:

```sh
cargo build --workspace --locked
cargo run --locked -- --help
```

To invoke the driver directly with three existing files:

```sh
cargo run --locked -- \
  -o base.rs -a ours.rs -b theirs.rs -p src/example.rs
```

This replaces `ours.rs` with conflict markers and returns `1`; it does not modify
`base.rs` or `theirs.rs`. `--marker-size` / `-l` accepts Git's `%L` marker length
(default `7`, range `1` through `65535`).

## Register with Git

Install the binary and configure the driver **locally in the repository that
should use it**:

```sh
cargo install --path crates/git-weld --locked
git config --local merge.git-weld.name "git-weld Semantic Merge Driver"
git config --local merge.git-weld.driver \
  'git-weld -o %O -a %A -b %B -p %P -l %L'
git config --local merge.git-weld.recursive binary
```

Ensure Cargo's binary directory is on `PATH`. This project's `.gitattributes`
already contains:

```gitattributes
*.rs merge=git-weld
*.py merge=git-weld
```

Copy those entries into another repository's `.gitattributes` when installing
there. Git substitutes and shell-quotes the `%O`, `%A`, `%B`, and `%P` paths.
`recursive = binary` keeps internal common-ancestor merges out of the custom
driver.

The initialized development repository uses a repository-local `cargo run`
driver so it works without installing a global binary. That configuration lives
in `.git/config`, not in tracked files; cloned repositories need the registration
above.

After a conflict, edit the marked file, remove the markers, and `git add` it.
Continue the merge or rebase as usual.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Git integration tests create disposable repositories with their own user and
merge-driver configuration; they do not change global Git settings. CI runs the
same checks.

```text
Cargo.toml                   Workspace and shared dependencies
crates/git-weld/src/main.rs   CLI, provider selection, diagnostics, exit codes
crates/git-weld/src/merge.rs  Manual-conflict formatting and atomic output
crates/git-weld/tests/       CLI and real Git merge integration tests
project_blueprint.md         Full architecture and development roadmap
```

## Next phases

Phase 2 adds `tree-sitter` parsing and intersecting AST node detection for Rust
and Python. Phase 3 introduces semantic state serialization,
`SystemOneProvider`, uniform decision types, an offline mock, and the first
verified provider integration. Phase 4 adds surgical auto-resolution only when
confidence is strictly greater than `0.95`, along with model-reasoning logs and
provider extension guidance. Until then, no merge is treated as model-approved.
