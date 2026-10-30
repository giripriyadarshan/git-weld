# Coding Standards

These standards govern code changes, reviews, and documentation in `git-weld`. Read [`GLOSSARY.md`](GLOSSARY.md), [`CONTRIBUTING.md`](CONTRIBUTING.md), [`docs/adr/0001-same-file-signature-hoisting.md`](docs/adr/0001-same-file-signature-hoisting.md), [`docs/adr/0002-sqlite-decision-cache.md`](docs/adr/0002-sqlite-decision-cache.md), and [`docs/adr/0003-ci-gated-crates-io-publishing.md`](docs/adr/0003-ci-gated-crates-io-publishing.md) alongside this file.

## 1. Fail-Safe Manual Fallback Invariant

- **Never corrupt or guess on a merge.** Every failure mode must degrade safely to standard diff3 conflict markers for **manual resolution** (exit code `1`), preserving non-overlapping line edits from `LineMerge`:
  - Unsupported file extensions, non-UTF-8 inputs, or syntax errors in **base**, **ours**, or **theirs** (`crates/git-weld/src/semantic.rs`).
  - **Conflicts** outside every syntactic **item** (`crates/git-weld/src/semantic.rs`).
  - Unknown or unconfigured **providers**, network/HTTP errors, timeouts, request size limits (`> 128 KiB`), or schema-invalid responses (`crates/git-weld/src/provider.rs`).
  - **Decisions** with action `Manual` or confidence `<= 0.95` (`AUTO_RESOLVE_THRESHOLD` is strictly `> 0.95`).
  - Stitched output that fails syntax verification in `SemanticMerge::resolve` (including Python `block` nodes with no non-comment statements), which rolls back all **chunks** in the file to **manual resolution** (`crates/git-weld/src/semantic.rs`).
- **Protect `%A` on driver errors.** Unreadable inputs, uncreatable output files, or invalid CLI flags must exit with code `2` without modifying `%A`. Write merged output atomically via `NamedTempFile` in the target directory while preserving file permissions (`crates/git-weld/src/merge.rs`).
- **Advisory side effects never alter merge outcomes.** Failures in the SQLite **decision cache** (`crates/git-weld/src/cache.rs`) or the `.git-weld.log` **decision log** (`crates/git-weld/src/log.rs`) must never change the merged text or exit code.
- **No unsafe code.** The workspace forbids `unsafe` (`unsafe_code = "forbid"` in `Cargo.toml`).

## 2. Domain Terminology (`GLOSSARY.md`)

Use the exact terms defined in [`GLOSSARY.md`](GLOSSARY.md) in types, identifiers, diagnostics, comments, and documentation:

- **`Version` vs `Side`**:
  - **Version** (`merge::Version`) is one of the three inputs to a three-way merge: `Base`, `Ours`, or `Theirs`. Never name another struct `Version` (for example, use `semantic::VersionState` for a chunk's per-version code slice), and never use `side` as an identifier when indexing or iterating over all three **versions**.
  - **Side** refers only to one of the two diverging versions being merged (`ours` or `theirs`), excluding `base`. Never use `branch` to mean a **side**.
- **`Conflict` vs `Chunk` vs `DivergedItem`** (never "hunk" for AST units):
  - **Conflict** is a region of overlapping, non-identical line edits from `LineMerge`.
  - **Chunk** (`semantic::Chunk`) is a contiguous span of merge regions widened to cover whole syntactic **items** in all three **versions**, evaluated and replaced as a unit.
  - **Diverged item** (`semantic::DivergedItem`) is an **item** that both **sides** changed differently relative to **base**, whether or not the line edits overlap.
- **`Item` vs Container**:
  - **Item** is a named syntactic declaration in one **version**, including its leading comments, attributes, and decorators.
  - Containers (`impl`, `trait`, `mod`, and `extern` blocks in Rust; `class` definitions in Python) have member **items** that are compared and chunked individually unless a **conflict** touches the container's own header or delimiters.
- **`Hoisted signature` (`SignatureState`)**:
  - Same-file declaration headers and member signatures (one level deep, executable bodies omitted) referenced by a **chunk**, per [ADR-0001](docs/adr/0001-same-file-signature-hoisting.md).
- **Qualify `manual` and `model`**:
  - Always distinguish the **manual provider** (`--provider manual`), a **Manual decision** (`MergeAction::Manual`), and **manual resolution** (leaving conflict markers for a person).
  - Distinguish the **configured model** (requested by configuration) from the **answering model** (reported in the provider's response).

## 3. Credential, Endpoint, and Cache Security

- **Environment injection (`provider::Env`) and empty-variable resilience.** Providers must read environment variables only through the `Env` closure passed to `from_env(env: Env)`, never by calling `std::env::var` directly inside provider modules (`crates/git-weld/src/provider.rs`). Empty or whitespace-only optional environment variables (including `WELD_NO_CACHE=""`) must be treated as unset rather than failing CLI parsing.
- **Never leak credentials.** Mark authorization headers with `HeaderValue::set_sensitive(true)` and implement `fmt::Debug` manually on every provider holding an API key so keys never appear in debug output, logs, or error messages.
- **HTTPS required for remote endpoints.** Reject plain `http://` URLs unless the host is loopback (`localhost`, `127.0.0.0/8`, or `::1`), and reject URLs containing embedded userinfo credentials, query strings, or fragments (`crates/git-weld/src/provider/jev.rs`, `crates/git-weld/src/provider/ollama.rs`).
- **Cache hygiene ([ADR-0002](docs/adr/0002-sqlite-decision-cache.md)).** Store only SHA-256 keys and **decisions** (never source code) in `<git-common-dir>/git-weld/cache`. Never cache answers produced when **hoisted signatures** had to be trimmed to fit the 128 KiB request limit.

## 4. Offline, Deterministic Testing

- **No network access or external credentials in tests.** Test HTTP providers against the local scripted server in `crates/git-weld/tests/support/mod.rs` and CLI workflows with `MockProvider` (`WELD_MOCK_DECISION`) or the scripted server.
- **Scrub ambient environment variables.** CLI and Git integration tests in `crates/git-weld/tests/merge_driver.rs` must clear `WELD_*`, `TYPESAFE_*`, and `OLLAMA_*` variables and isolate Git configuration (`GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL` pointing to a nonexistent path) in disposable `tempfile::TempDir` repositories.
- **Test at the highest seam first.** Prefer end-to-end CLI and real `git merge` / `git rebase` / `git worktree` integration tests, supplemented by focused unit and property tests for pure invariants (`LineMerge` consistency, chunk item-boundary invariants, signature hoisting, and cache key/TTL rules).

## 5. Prose and Documentation Style

- **No em dashes or en dashes.** Never use the em dash (`U+2014`) or en dash (`U+2013`) in documentation, comments, commit messages, or diagnostics. Use colons, semicolons, parentheses, commas, or separate sentences instead.
- **Required verification.** Every change must pass all four checks in [`CONTRIBUTING.md`](CONTRIBUTING.md):
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -- -D warnings`
  - `cargo test --workspace --locked`
  - `cargo audit --deny warnings`

## 6. Release and Changelog Discipline ([ADR-0003](docs/adr/0003-ci-gated-crates-io-publishing.md))

- **Audit dependencies before and without releasing.** `.github/workflows/ci.yml` must run a `security-audit` job (`cargo audit --deny warnings`, plus `actions/dependency-review-action` on pull requests) on `push`, `pull_request`, `workflow_dispatch`, and a daily `schedule` so `Cargo.lock` advisories and GitHub Advisory Database findings can be audited continuously and on demand without cutting a release. `.github/workflows/release.yml` must also gate `package` behind an `audit` job (`cargo audit --deny warnings`) so a release tag with known vulnerabilities or unsoundness warnings is never packaged or published.
- **Never run `cargo publish` locally during release preparation.** `[workspace.metadata.release]` in `Cargo.toml` must keep `publish = false` so `cargo-release` only updates the workspace version, runs `git-cliff` to update `CHANGELOG.md`, commits `Prepare v<version>`, and pushes the `v<version>` tag. Crates.io publishing is gated behind the `release` job in `.github/workflows/release.yml` so `cargo-binstall` binary archives are always live on GitHub Releases before the crate version appears on crates.io.
- **Single source of truth for release notes.** `cliff.toml` governs both the committed `CHANGELOG.md` (via `cargo-release`'s `pre-release-hook`) and the GitHub Release notes generated in `.github/workflows/release.yml` (`git-cliff --current --strip all`). Keep `cliff.toml` templates and commit parsers free of em dashes and en dashes.

