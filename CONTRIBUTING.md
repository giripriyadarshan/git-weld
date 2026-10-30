# Contributing to git-weld

## Checks

CI runs these four commands, and every change should pass them locally:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo audit --deny warnings
```

The tests need Git, because some of them run real `git merge` commands in
disposable repositories and compare the line merge with `git merge-file`.
They never reach the network.

## Adding a provider

git-weld talks to models through the `SystemOneProvider` trait in
`crates/git-weld/src/provider.rs`. A provider receives the semantic state of
one file, holding every chunk the driver wants decided, and returns one
`Decision` per chunk. Everything around that is the driver's job: the line
merge, chunking, the `0.95` confidence gate, stitching, conflict markers, the
parse check, and the decision log. A provider only turns chunks into
decisions.

The steps below use Cloudflare's Clef as the example, one of the planned
providers.

### 1. Implement the trait

Create `crates/git-weld/src/provider/clef.rs`:

```rust
use super::{Env, Evaluation, ProviderError, SystemOneProvider};
use crate::state::SemanticState;
use async_trait::async_trait;

pub struct ClefProvider {
    // HTTP client, endpoint, credentials, model name, ...
}

impl ClefProvider {
    pub const NAME: &str = "clef";

    /// Reads the provider's settings through `env`.
    pub fn from_env(env: Env) -> Result<Self, ProviderError> {
        let api_key = env("CLEF_API_KEY")
            .ok_or_else(|| ProviderError::Config("CLEF_API_KEY is not set".to_owned()))?;
        todo!("validate the settings and build the client with {api_key}")
    }
}

#[async_trait]
impl SystemOneProvider for ClefProvider {
    fn name(&self) -> &str {
        Self::NAME
    }

    async fn evaluate(&self, state: &SemanticState) -> Result<Evaluation, ProviderError> {
        todo!("ask about all {} chunks and return one decision each", state.chunks.len())
    }
}
```

The trait uses `#[async_trait]` because the driver holds providers as
`Box<dyn SystemOneProvider>`. Keep these rules in mind:

- **One decision per chunk, in order.** `evaluate` returns an `Evaluation`
  whose `decisions` holds one decision for each entry of `state.chunks` and
  whose `trimmed` flag reports whether hoisted signatures had to be trimmed.
  `provider::evaluate` wraps your method: it rejects any other count and
  applies the `--timeout` limit to the whole evaluation, so do not add an
  overall timeout of your own.
- **Ask about all chunks at once.** A file with three conflicts should cost
  one round trip. Send all chunks in one request when the backend supports
  batches, as Jev does with one question per chunk. Otherwise ask about them
  concurrently rather than one after another, since they share one time
  limit.
- **Keep the meaning of the decisions.** `TakeOurs` means "ours' version of
  the chunk exactly as it is, which already includes theirs' change or an
  equivalent one, so nothing from theirs is lost", and `TakeTheirs` mirrors
  it. When neither side alone is right, the answer is `Manual`. The question
  and criteria in `provider/http.rs` spell this out and make a good starting
  point for a prompt.
- **Never guess.** A decision has a `decision` (`TakeOurs`, `TakeTheirs`, or
  `Manual`), a `confidence` from 0 to 1, and a `reason`. Backends that
  generate text can constrain their output with `Decision::json_schema()` and
  decode it with `serde_json`, which enforces the same rules. When an answer
  does not fit, return `ProviderError::InvalidResponse` instead of repairing
  it: every error leaves the chunks to a person, which is always safe.
- **Use the state as it is.** `SemanticState` serializes to the JSON that Jev
  and Ollama receive and displays as plain text for prompt-based backends. Each
  chunk carries its items, a neutral summary, and its code in each version with
  the diffs from base; a missing version means the chunk is empty on that
  side. `state.signatures` holds the hoisted signatures of items in the same
  file that the chunks reference.
- **Opt in to the decision cache.** Implement `cache_key(&self) -> Option<String>`
  to return a string combining your provider's name, its configured model, and
  a prompt revision, and return `Evaluation::trimmed(decisions)` (setting
  `trimmed: true`) when `evaluate` trims `state.signatures` to fit a request
  size limit so `provider::evaluate_with_cache` never caches trimmed-context
  answers. Leave the default `None` for test-only providers that should not
  write to `.git/git-weld/cache`.
- **Protect credentials.** Read settings only through `env`, never through
  `std::env`, so tests can pass their own. Do not log or print keys; if the
  struct holds one, implement `Debug` by hand as `JevProvider` does. Require
  HTTPS for remote endpoints.

### 2. Register the name

In `crates/git-weld/src/provider.rs`:

1. Declare the module next to the others and export the type:
   `mod clef;` and `pub use clef::ClefProvider;`.
2. Add `ClefProvider::NAME` to `PROVIDERS`, in alphabetical order. The list
   appears in the error for unknown names.
3. Add a branch to `load` that builds the provider:

   ```rust
   } else if name.eq_ignore_ascii_case(ClefProvider::NAME) {
       boxed(ClefProvider::NAME, ClefProvider::from_env(env))
   ```

Names are matched ignoring ASCII case, and a provider that fails to load is
reported on stderr while every chunk keeps its markers. Update the
`loads_providers_by_name` test, which currently expects `clef` to be unknown,
and the expected provider lists in `tests/merge_driver.rs`.

### 3. Test offline

Tests must not reach the network or need real credentials.

- **Unit tests** in the provider's module: build a `SemanticState` (the
  `state` helpers in the tests of `provider.rs` and `provider/http.rs` show two
  ways), then check the request you would send and the decisions you decode,
  including answers that must be rejected.
- **HTTP behavior:** point the provider at the scripted server in
  `crates/git-weld/tests/support/mod.rs`, as `tests/jev_provider.rs` does:
  `Server::start(vec![Response::new(200, body)])` serves canned responses on
  localhost and records every request, so you can check the request shape,
  authentication, error handling, retries, and timeouts.
- **End to end:** `tests/merge_driver.rs` runs the real binary, including
  through `git merge`. The `mock` provider, which answers from
  `WELD_MOCK_DECISION`, shows how the driver acts on decisions without any
  backend.

### 4. Document it

Add a row to the Providers table in the README with the provider's
settings and their defaults, and say what leaves the machine: with any
remote provider, the code of every conflicting chunk is sent to it.

## Releasing

Releases are prepared locally with [`cargo-release`](https://github.com/crate-ci/cargo-release) and [`git-cliff`](https://github.com/orhun/git-cliff) and published by `.github/workflows/release.yml` ([ADR-0003](docs/adr/0003-ci-gated-crates-io-publishing.md)):

1. **Configure local release preparation (`[workspace.metadata.release]` in `Cargo.toml` and `cliff.toml`):**
   - `cargo-release` shares the workspace version (`shared-version = true`), disables local crates.io publishing (`publish = false`), formats tags as `v{{version}}`, commits with `Prepare v{{version}}`, and runs `git-cliff --workdir ../.. --config cliff.toml -o ../../CHANGELOG.md --tag v{{version}}` as its `pre-release-hook`.
   - `cliff.toml` parses both Conventional Commits (`feat:`, `fix:`, `doc:`, `perf:`, `refactor:`, `test:`, `chore:`) and `git-weld`'s imperative commit subjects (`Add ...`, `Implement ...`, `Fix ...`, `Document ...`) while skipping release-preparation commits (`Prepare v*`, `chore(release):*`) and bootstrap noise.
2. **Verify dependency security and preview the release bump:**
   ```sh
   # Verify zero RustSec vulnerabilities/warnings and zero open Dependabot alerts before releasing
   cargo audit --deny warnings
   gh api repos/giripriyadarshan/git-weld/dependabot/alerts --jq 'map(select(.state == "open")) | length'

   # Preview version bump and CHANGELOG.md generation (dry run), then restore CHANGELOG.md
   cargo release <rc|release|patch|minor|major>
   git checkout CHANGELOG.md

   # Apply the bump, update CHANGELOG.md, commit, tag v<version>, and push
   cargo release <rc|release|patch|minor|major> --execute
   ```
3. **Automated CI packaging and publishing (`.github/workflows/release.yml`):**
   - Pushing a `v*` tag triggers the `audit` job (`cargo audit --deny warnings`), which gates the entire workflow so a tag with known lockfile vulnerabilities or unsoundness warnings cannot be packaged or published.
   - Once `audit` succeeds, the `package` matrix job verifies that `GITHUB_REF_NAME` matches `Cargo.toml`, builds release archives across all 7 targets, and tests each archive offline with `cargo-binstall`.
   - Once all target archives succeed, the `release` job aggregates `SHA256SUMS`, extracts the current version's section with `git-cliff --tag "${GITHUB_REF_NAME}" --current --strip all`, and creates the GitHub Release (marking pre-release tags containing `-` with `--prerelease`).
   - Finally, the `publish` job authenticates with crates.io using OIDC Trusted Publishing (`rust-lang/crates-io-auth-action@v1`) and runs `cargo publish --locked --package git-weld` only after the GitHub Release assets required by `cargo-binstall` are live.

