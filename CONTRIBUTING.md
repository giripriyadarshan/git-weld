# Contributing to git-weld

## Checks

CI runs these three commands, and every change should pass them locally:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
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
use super::{Decision, Env, ProviderError, SystemOneProvider};
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

    async fn evaluate(&self, state: &SemanticState) -> Result<Vec<Decision>, ProviderError> {
        todo!("ask about all {} chunks and return one decision each", state.chunks.len())
    }
}
```

The trait uses `#[async_trait]` because the driver holds providers as
`Box<dyn SystemOneProvider>`. Keep these rules in mind:

- **One decision per chunk, in order.** `evaluate` returns exactly one
  decision for each entry of `state.chunks`. `provider::evaluate` wraps your
  method: it rejects any other count and applies the `--timeout` limit to the
  whole evaluation, so do not add an overall timeout of your own.
- **Ask about all chunks at once.** A file with three conflicts should cost
  one round trip. Send all chunks in one request when the backend supports
  batches, as Jev does with one question per chunk. Otherwise ask about them
  concurrently rather than one after another, since they share one time
  limit.
- **Keep the meaning of the decisions.** `TakeOurs` means "ours' version of
  the chunk exactly as it is, which already includes theirs' change or an
  equivalent one, so nothing from theirs is lost", and `TakeTheirs` mirrors
  it. When neither side alone is right, the answer is `Manual`. The question
  and criteria in `provider/jev.rs` spell this out and make a good starting
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
  a prompt revision, and implement `would_trim(&self, state: &SemanticState) -> bool`
  if your provider trims `state.signatures` to fit a request size limit (so
  trimmed-context answers are never cached). Leave the default `None` for
  test-only providers that should not write to `.git/git-weld/cache`.
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
  `state` helpers in the tests of `provider.rs` and `provider/jev.rs` show two
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
