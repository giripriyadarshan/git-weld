# git-weld

A model-agnostic semantic Git merge driver in Rust.

## Current scope

Phases 1 to 3 are implemented: **Foundation & Git Hook Plumbing**, the **AST
parsing engine**, and the **System One provider abstraction**. The Rust 2024
workspace contains a `clap` CLI, a merge module, a `tree-sitter` analysis
module, a semantic state builder, a provider layer with a client for TypeSafe
AI's Jev and an offline mock, and real Git integration tests for Rust and Python
files.

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

When a [provider](#providers) is configured and at least one item changed on
both sides, the driver describes those items neutrally (each version's code
plus a unified diff from the base) and asks the provider how to resolve each
one. The answers are reported, but in this build they are advisory:

```text
git-weld: jev decided 1 item:
git-weld:   impl Counter > fn get: TakeTheirs, confidence 0.97, above the 0.95 threshold: jev-1.13.0 chose TakeTheirs (probabilities: Manual 0.01, TakeOurs 0.01, TakeTheirs 0.98)
git-weld: automatic resolution is not implemented yet; the conflict is left for manual resolution
```

Every provider answers in the same shape, one decision per item:

```json
{"decision": "TakeOurs", "confidence": 0.98, "reason": "..."}
```

`decision` is `TakeOurs`, `TakeTheirs`, or `Manual`, and `confidence` is a
number from 0 to 1. Unknown fields or decisions and out-of-range confidences
are rejected; `Decision::json_schema()` publishes the same rules as a JSON
Schema. A decision counts as confident only when it picks a side with a
confidence strictly greater than `0.95`; exactly `0.95` is not enough.

The report is only a diagnostic. An unsupported file type, input that is not
valid UTF-8, or a syntax error in any version skips it with a note on stderr,
and so does any provider problem: an unknown or unconfigured provider, a
refused connection, an HTTP error, a timeout, or an answer that breaks the
decision schema. The merge result and exit code are the same either way.

No automatic resolutions or `.git-weld.log` generation are implemented yet.
Even changes that a normal text merge could combine, or that touch different
items, are left for manual resolution in this build.

## Providers

Choose a provider with `--provider` or `WELD_PROVIDER`. The flag takes
precedence over the environment, names ignore case, and the default is
`manual`.

| Provider | Behavior | Settings |
|----------|----------|----------|
| `manual` | No model is consulted. | None |
| `jev` | Asks TypeSafe AI's [Jev](https://docs.typesafe.ai) System One model. | `TYPESAFE_API_KEY` (required), `TYPESAFE_BASE_URL` (default `https://api.typesafe.ai`), `TYPESAFE_DEFAULT_MODEL` (default `jev-latest`) |
| `mock` | Answers from `WELD_MOCK_DECISION` without a model, for offline tests. | `WELD_MOCK_DECISION`: one decision object for every item, or an array with one decision per item |

`--timeout` or `WELD_TIMEOUT` sets how many seconds to wait for an answer
(default `10`, at most `3600`). Other names, including the planned `clef` and
`sap-rst`, are reported as unknown.

The `jev` provider sends one `POST /v1/systemone` request per conflicted file,
using the same variable names as TypeSafe's SDKs. The state holds the file's
logical path and language and, for every item that both sides changed, its
code in each version plus the diffs from the base. Each item becomes one Choice
question with the options `Manual`, `TakeOurs`, and `TakeTheirs`; Jev's chosen
option and calibrated confidence become the decision, and the reason records
the answering model and its probabilities. The base URL must use HTTPS (plain
HTTP is accepted only for localhost), requests over 128 KiB are not sent, and
rate limits, overload, server errors, and refused connections are retried
twice unless the service asks to wait more than two seconds.

With `jev`, the code of every conflicting item leaves your machine. Review
TypeSafe's data handling terms before enabling it for private code.

## Build and run

Install a stable Rust toolchain (1.90 or newer, required by `tree-sitter`), a C
compiler (the bundled grammars and the `aws-lc` TLS library are compiled from
C), and Git, then run:

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
there. To consult a model, append `--provider jev` to the driver command or
export `WELD_PROVIDER=jev` in the environment that runs Git. Export
`TYPESAFE_API_KEY` there as well; keep API keys out of Git configuration. Git substitutes and shell-quotes the `%O`, `%A`, `%B`, and `%P` paths.
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
merge-driver configuration; they do not change global Git settings. Provider
tests never reach the network: Jev is tested against a scripted HTTP server on
localhost, and the CLI tests ignore any `WELD_*` or `TYPESAFE_*` variables set
in your shell. CI runs the same checks.

```text
Cargo.toml                        Workspace and shared dependencies
crates/git-weld/src/main.rs       CLI, provider loading, diagnostics, exit codes
crates/git-weld/src/merge.rs      Manual-conflict formatting and atomic output
crates/git-weld/src/ast.rs        Language selection, parsing, three-way item comparison
crates/git-weld/src/ast/items.rs  Item extraction and formatting-insensitive fingerprints
crates/git-weld/src/state.rs      Semantic state: neutral plain-text and JSON descriptions
crates/git-weld/src/provider.rs   Decision types and schema, SystemOneProvider, loading
crates/git-weld/src/provider/     Jev (TypeSafe API) and mock providers
crates/git-weld/tests/            CLI, Jev, and real Git merge integration tests
```

## Next phases

Phase 4 first merges each file in memory, the way `git merge-file` does, so
edits that do not overlap combine without a model. Each remaining conflict is
narrowed to the smallest enclosing item, all of a file's conflicts go to the
provider in one request, and each one is decided on its own: a confident
decision (confidence strictly greater than `0.95`) replaces just that item,
and the rest keep standard conflict markers. Phase 4 also adds a
`.git-weld.log` with each model's reasoning and a guide for adding providers.
Phase 5 gives the model the signatures of the types and functions each
conflict refers to, and caches decisions in `.git/git-weld/cache` so a rebase
that replays the same conflict does not ask the provider again. Until then, no
merge is treated as model-approved.
