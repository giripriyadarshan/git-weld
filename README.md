# git-weld

A model-agnostic semantic Git merge driver in Rust.

## Current scope

Phases 1 to 5 are implemented: **Foundation & Git Hook Plumbing**, the **AST
parsing engine**, the **System One provider abstraction**, **chunked
auto-resolution**, and **context hoisting with an idempotent decision cache**.
The Rust 2024 workspace contains a `clap` CLI, an in-memory line merge, a
`tree-sitter` analysis that turns conflicts into chunks of whole items and
hoists referenced signatures, a semantic state builder, a provider layer with
clients for TypeSafe AI's Jev, Ollama's System One models (`kev`, `laya`,
`clef`, and others), and an offline mock, a SQLite decision cache, a decision
log for editors, and real Git integration tests for Rust and Python files.

### How a merge runs

1. **Line merge.** The driver reads the base, ours, and theirs versions and
   merges them in memory by lines, like `git merge-file` with the histogram
   diff that Git's own merge uses. Edits that do not overlap combine, and
   identical edits count once. A file without conflicts is written as merged
   and the driver exits with code `0` without consulting a model, as Git's own
   merge would.
2. **Chunks and hoisted signatures.** The driver parses all three versions with
   `tree-sitter`, choosing the grammar from the `%P` extension (`.rs` or
   `.py`), and widens every remaining conflict to the smallest enclosing items.
   A conflict inside a function covers that function, a conflict spanning two
   adjacent items covers both, and conflicts inside the same item share one
   chunk. Members of `impl`, `trait`, `mod`, and `extern` blocks and of Python
   classes are chunks of their own unless the conflict touches the container
   itself, such as its header. Items include their leading comments,
   attributes, and decorators. A `tree-sitter` query also collects the types,
   traits, classes, functions, and methods each chunk references in the same
   file and hoists their signatures (without executable bodies) into the
   semantic state.
3. **Decision cache and provider.** Before calling a remote or local model,
   each chunk is looked up in `.git/git-weld/cache` (under
   `git rev-parse --git-common-dir`, so linked worktrees share it) by a
   SHA-256 hash of the provider, the configured model, the language, the
   chunk's Base, Ours, and Theirs code, and the hoisted signatures that chunk
   references. Cache hits are reused immediately; only misses go to the
   configured [provider](#providers) in a single request, and valid answers
   (when no hoisted signatures had to be trimmed) are stored for 30 days.
   `--no-cache` or `WELD_NO_CACHE` bypasses the cache.
4. **Stitching.** A chunk whose decision is `TakeOurs` or `TakeTheirs` with a
   confidence strictly greater than `0.95` is replaced by that side's code for
   the whole chunk; exactly `0.95` is not enough. Every other conflict keeps
   standard conflict markers around that conflict only, while confident
   chunks in the same file stay resolved. If the stitched file does not parse,
   every chunk keeps its markers. The driver exits with code `0` when no
   markers are left and `1` otherwise.

For a file with three conflicts, the driver reports:

```text
git-weld: src/lib.rs: 3 conflicts in 3 chunks (provider: jev):
git-weld:   chunk 1: fn alpha
git-weld:   chunk 2: impl Counter > fn get
git-weld:   chunk 3: fn gamma
git-weld: jev decided 3 chunks:
git-weld:   chunk 1: TakeOurs, confidence 0.98, above the 0.95 threshold: jev-1.13.0 chose TakeOurs (probabilities: Manual 0.01, TakeOurs 0.98, TakeTheirs 0.01)
git-weld:   chunk 2: Manual, confidence 0.72: jev-1.13.0 chose Manual (probabilities: Manual 0.72, TakeOurs 0.2, TakeTheirs 0.08)
git-weld:   chunk 3: TakeTheirs, confidence 0.97, above the 0.95 threshold: jev-1.13.0 chose TakeTheirs (probabilities: Manual 0.02, TakeOurs 0.01, TakeTheirs 0.97)
git-weld: resolved 2 of 3 chunks; 1 left for manual resolution, at lines 9-19
```

Conflict markers use the diff3 style with the labels `HEAD`, `base`, and
`theirs` and honor Git's `%L` marker size. Like Git, the driver ends markers
in CRLF when the surrounding lines do and adds a missing final newline inside
a conflict, so markers always start on their own line. Replacement is atomic
and preserves the output file's permissions. Input or output errors are
reported on stderr and exit with code `2`, leaving `%A` untouched.

Every problem falls back to the line merge's markers: an unsupported file
type, input that is not valid UTF-8, a syntax error in any version, a conflict
outside every item (such as a comment at the end of the file), an unknown or
unconfigured provider, a refused connection, an HTTP error, a timeout, an
answer that breaks the decision schema, a decision that is `Manual` or not
confident enough, and a stitched file that does not parse. A Python block
without statements counts as a syntax error, as it does in Python.

When both sides changed the same item but their edits do not overlap, the
line merge combines them without asking a model, just like Git. Combined
edits can still clash in meaning, so the driver points such items out:

```text
git-weld: note: both sides changed fn alpha, but the edits do not overlap, so they were merged line by line
```

These notes ignore formatting-only edits (whitespace, indentation, line
breaks, spacing inside comments, line endings, and optional trailing commas)
and identical edits. String literals are compared exactly.

Every provider answers in the same shape, one decision per chunk:

```json
{"decision": "TakeOurs", "confidence": 0.98, "reason": "..."}
```

`decision` is `TakeOurs`, `TakeTheirs`, or `Manual`, and `confidence` is a
number from 0 to 1. Unknown fields or decisions and out-of-range confidences
are rejected; `Decision::json_schema()` publishes the same rules as a JSON
Schema.

### Decision log

Each decision is appended to `.git-weld.log` in the driver's working
directory, which is the top of the work tree when Git runs it. Every line is
one JSON object, so editor plugins can show the reasoning next to the code:

```json
{"time":"2026-10-12T14:42:27Z","file":"src/lib.rs","chunk":2,"items":["impl Counter > fn get"],"lines":{"start":9,"end":19},"provider":"jev","decision":"Manual","confidence":0.72,"reason":"jev-1.13.0 chose Manual (probabilities: Manual 0.72, TakeOurs 0.2, TakeTheirs 0.08)","resolved":false,"cached":false}
```

`time` is in UTC, `chunk` counts from 1 within the file, `lines` is where the
chunk is in the merged file (the chosen side's code when `resolved` is true,
the conflict markers otherwise), and `cached` is true when the decision came
from `.git/git-weld/cache`. Records are only ever appended, so the newest
record for a file and chunk is the one that applies. A log that cannot be
written is reported on stderr and never changes the merge result.

## Providers

Choose a provider with `--provider` or `WELD_PROVIDER`. The flag takes
precedence over the environment, names ignore case, and the default is
`manual`.

| Provider | Behavior | Settings |
|----------|----------|----------|
| `manual` | No model is consulted. | None |
| `jev` | Asks TypeSafe AI's [Jev](https://docs.typesafe.ai) System One model. | `TYPESAFE_API_KEY` (required), `TYPESAFE_BASE_URL` (default `https://api.typesafe.ai`), `TYPESAFE_DEFAULT_MODEL` (default `jev-latest`) |
| `ollama` | Asks a System One model served by [Ollama](https://ollama.com) (`kev`, `laya`, `clef`, and others). | `OLLAMA_MODEL` (required, e.g. `kev:0.8b-bf16` or `laya:latest`), `OLLAMA_HOST` (default `http://127.0.0.1:11434`), `OLLAMA_API_KEY` (optional) |
| `mock` | Answers from `WELD_MOCK_DECISION` without a model, for offline tests. | `WELD_MOCK_DECISION`: one decision object for every chunk, or an array with one decision per chunk |

`--timeout` or `WELD_TIMEOUT` sets how many seconds to wait for an answer
(default `10`, at most `3600`). `--no-cache` or `WELD_NO_CACHE` skips reading
and writing `.git/git-weld/cache`. Other provider names are reported as
unknown. [CONTRIBUTING.md](CONTRIBUTING.md) explains how to add a provider.

Both `jev` and `ollama` send one `POST /v1/systemone` request per conflicted
file. The state holds the file's logical path, its language, the hoisted
signatures of items in the same file that the chunks reference, and, for every
chunk, its items, a neutral summary, its code in each version, and the diffs
from the base. Each chunk becomes one Choice question with the options
`Manual`, `TakeOurs`, and `TakeTheirs`, evaluated in parallel against the
state. When hoisted signatures would push a request over 128 KiB, the largest
signatures are trimmed before any chunk; requests whose chunks alone exceed
128 KiB are not sent. The base URL must use HTTPS (plain HTTP is accepted only
for localhost), and rate limits, overload, server errors, and refused
connections are retried twice unless the service asks to wait more than two
seconds.

With `jev` or a remote `ollama` host, the code of every conflicting chunk and
its hoisted signatures leaves your machine. Review the service's data handling
terms before enabling it for private code.

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

This merges the three versions into `ours.rs` and returns `0` when no conflict
markers are left or `1` when some are; it does not modify `base.rs` or
`theirs.rs`. `--marker-size` / `-l` accepts Git's `%L` marker length (default
`7`, range `1` through `65535`).

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
`TYPESAFE_API_KEY` there as well; keep API keys out of Git configuration. Git
substitutes and shell-quotes the `%O`, `%A`, `%B`, and `%P` paths.
`recursive = binary` keeps internal common-ancestor merges out of the custom
driver. Add `.git-weld.log` to the repository's `.gitignore` or
`.git/info/exclude` so the decision log is never committed.

The initialized development repository uses a repository-local `cargo run`
driver so it works without installing a global binary. That configuration lives
in `.git/config`, not in tracked files; cloned repositories need the registration
above.

After a conflict, edit the marked chunks, remove the markers, and `git add` the
file. Continue the merge or rebase as usual.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Git integration tests create disposable repositories with their own user and
merge-driver configuration; they do not change global Git settings. Provider
tests never reach the network: `jev` and `ollama` are tested against a
scripted HTTP server on localhost, and the CLI tests ignore any `WELD_*`,
`TYPESAFE_*`, or `OLLAMA_*` variables set in your shell. The line merge is
compared with `git merge-file`, so Git must be installed. CI runs the same
checks.

```text
Cargo.toml                        Workspace and shared dependencies
GLOSSARY.md                       Canonical domain terms
CODING_STANDARDS.md               Coding standards and review invariants
CONTRIBUTING.md                   Checks and how to add a provider
docs/adr/                         Architecture decision records
crates/git-weld/src/main.rs       CLI, merge data flow, diagnostics, exit codes
crates/git-weld/src/merge.rs      Line merge, conflict markers, and atomic output
crates/git-weld/src/ast.rs        Language selection, parsing, item outlines, item comparison
crates/git-weld/src/ast/items.rs  Item extraction, signatures, reference queries, fingerprints
crates/git-weld/src/chunk.rs      Widening conflicts to chunks of whole items
crates/git-weld/src/state.rs      Semantic state: chunks and hoisted signatures
crates/git-weld/src/cache.rs      The .git/git-weld/cache SQLite decision cache
crates/git-weld/src/provider.rs   Decision types and schema, SystemOneProvider, loading
crates/git-weld/src/provider/     Jev (TypeSafe API), Ollama, and mock providers
crates/git-weld/src/log.rs        The .git-weld.log decision log
crates/git-weld/tests/            CLI, Jev, Ollama, cache, and real Git integration tests
```
