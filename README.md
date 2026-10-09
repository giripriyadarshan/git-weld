# git-weld

A model-agnostic semantic Git merge driver in Rust.

## Current scope

Phases 1 and 2 are implemented: **Foundation & Git Hook Plumbing** and the
**AST parsing engine**. The Rust 2024 workspace contains a `clap` CLI, a merge
module, a `tree-sitter` analysis module, and real Git integration tests for Rust
and Python files.

The driver still **always requires manual resolution**. It reads the base,
ours, and theirs files, writes a full-file diff3 conflict into `%A`, reports the
input paths on stderr, and exits with code `1`. All three versions are included;
missing final newlines are added so markers stay on separate lines. Replacement
is atomic and preserves the output file's permissions. Input or output errors
are reported on stderr and exit with code `2`.

After writing the conflict, the driver parses all three versions with
`tree-sitter`, choosing the grammar from the `%P` extension (`.rs` or `.py`), and
lists the items that both sides changed relative to the base:

```text
git-weld: 1 Rust item changed on both sides:
git-weld:   impl Counter > fn get: modified on both sides (base lines 6-8, ours lines 6-8, theirs lines 6-9)
```

Items are declarations such as functions, structs, enums, traits, classes,
imports, and module-level statements. Methods and other members of `impl`,
`trait`, `mod`, and `extern` blocks and of Python classes are compared one by
one, so edits to two different methods do not count as a conflict. Comments,
attributes, and decorators belong to the item that follows them. An item is
reported when both sides modified it differently, both added it with different
content, or one side deleted it while the other changed it. Identical edits are
not reported, and neither are formatting-only edits: whitespace, indentation,
line breaks, spacing inside comments, line endings, and optional trailing
commas. Reordering items with different names is not a change either. String
literals are compared exactly.

The report is only a diagnostic. An unsupported file type, input that is not
valid UTF-8, or a syntax error in any version skips it with a note on stderr,
and the merge result and exit code are the same either way.

No remote model calls, automatic resolutions, or `.git-weld.log` generation are
implemented yet. `--provider` and `WELD_PROVIDER` capture the requested provider
only; choosing `jev`, `clef`, or `sap-rst` does not enable a backend. A CLI flag
takes precedence over the environment, and the default is `manual`. Even
changes that a normal text merge could combine, or that touch different items,
are left for manual resolution in this build.

## Build and run

Install a stable Rust toolchain (1.90 or newer, required by `tree-sitter`), a C
compiler (the bundled grammars are compiled from C), and Git, then run:

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
Cargo.toml                       Workspace and shared dependencies
crates/git-weld/src/main.rs      CLI, provider selection, diagnostics, exit codes
crates/git-weld/src/merge.rs     Manual-conflict formatting and atomic output
crates/git-weld/src/ast.rs       Language selection, parsing, three-way item comparison
crates/git-weld/src/ast/items.rs Item extraction and formatting-insensitive fingerprints
crates/git-weld/tests/           CLI and real Git merge integration tests
```

## Next phases

Phase 3 introduces semantic state serialization, `SystemOneProvider`, uniform
decision types, an offline mock, and the first verified provider integration.
Phase 4 adds surgical auto-resolution only when confidence is strictly greater
than `0.95`, along with model-reasoning logs and provider extension guidance.
Until then, no merge is treated as model-approved.
