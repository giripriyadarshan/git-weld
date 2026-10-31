//! The SQLite decision cache in `<git-common-dir>/git-weld/cache`.
//!
//! Each decidable chunk is keyed by a SHA-256 digest over the provider's cache
//! identity, the file's language, the chunk's normalized Base, Ours, and
//! Theirs code, and the hoisted signatures that chunk references. File paths
//! and line numbers are excluded so moved conflicts still hit, while Ours and
//! Theirs stay in order so swapping sides never reuses `TakeOurs` or
//! `TakeTheirs` backwards. Any cache failure counts as a silent miss.

use crate::{
    provider::{Confidence, Decision, MergeAction},
    state::SemanticState,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Path of the SQLite database inside the shared Git directory.
pub const RELATIVE_PATH: &str = "git-weld/cache";

/// Entries expire 30 days after creation so moving model aliases such as
/// `jev-latest` periodically refresh.
pub const TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

const CACHE_KEY_VERSION: &[u8] = b"git-weld-cache-v1";
const BUSY_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Cache {
    connection: Connection,
}

impl Cache {
    /// Opens the decision cache for the Git repository containing `work_dir`.
    /// Returns `None` when `work_dir` is not inside a Git repository or the
    /// database cannot be opened.
    pub fn open_in_repo(work_dir: &Path) -> Option<Self> {
        let path = locate(work_dir)?;
        Self::open(&path)
    }

    /// Opens or initializes the SQLite decision cache at `path`.
    pub fn open(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok()?;
        }
        let connection = Connection::open(path).ok()?;
        connection.busy_timeout(BUSY_TIMEOUT).ok()?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE IF NOT EXISTS decisions (
                     key BLOB PRIMARY KEY,
                     decision TEXT NOT NULL,
                     confidence REAL NOT NULL,
                     reason TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 ) STRICT;",
            )
            .ok()?;
        Some(Self { connection })
    }

    /// Looks up a non-expired decision for `key`.
    pub fn get(&self, key: &[u8; 32]) -> Option<Decision> {
        self.get_at(key, now_seconds())
    }

    /// Stores `decision` under `key` and prunes expired rows.
    pub fn put(&self, key: &[u8; 32], decision: &Decision) {
        self.put_at(key, decision, now_seconds());
    }

    fn get_at(&self, key: &[u8; 32], now: u64) -> Option<Decision> {
        let row: Option<(String, f64, String, i64)> = self
            .connection
            .query_row(
                "SELECT decision, confidence, reason, created_at
                 FROM decisions
                 WHERE key = ?1",
                params![key.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .ok()?;
        let (action, confidence, reason, created_at) = row?;
        let created_at = u64::try_from(created_at).ok()?;
        if now.saturating_sub(created_at) >= TTL_SECONDS {
            return None;
        }
        let action = action.parse::<MergeAction>().ok()?;
        let confidence = Confidence::new(confidence).ok()?;
        Some(Decision::new(action, confidence, reason))
    }

    fn put_at(&self, key: &[u8; 32], decision: &Decision, now: u64) {
        let cutoff = i64::try_from(now.saturating_sub(TTL_SECONDS)).unwrap_or(0);
        let _ = self.connection.execute(
            "DELETE FROM decisions WHERE created_at <= ?1",
            params![cutoff],
        );
        let now = i64::try_from(now).unwrap_or(i64::MAX);
        let _ = self.connection.execute(
            "INSERT OR REPLACE INTO decisions (key, decision, confidence, reason, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.as_slice(),
                decision.decision.as_str(),
                decision.confidence.value(),
                decision.reason.as_str(),
                now,
            ],
        );
    }
}

/// Resolves `<git-common-dir>/git-weld/cache` for `work_dir`, or `None` when
/// `work_dir` is outside a Git repository.
pub fn locate(work_dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .current_dir(work_dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(["rev-parse", "--git-common-dir"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = std::str::from_utf8(&output.stdout)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    if raw.is_empty() {
        return None;
    }
    let common_dir = work_dir.join(raw);
    common_dir.is_dir().then(|| common_dir.join(RELATIVE_PATH))
}

/// Computes the SHA-256 cache key for `state.chunks[chunk_index]` and the
/// hoisted signatures it references.
pub(crate) fn chunk_key(provider_key: &str, state: &SemanticState, chunk_index: usize) -> [u8; 32] {
    let chunk = &state.chunks[chunk_index];
    let mut hasher = Sha256::new();
    write_bytes(&mut hasher, b'V', CACHE_KEY_VERSION);
    write_str(&mut hasher, b'P', provider_key);
    write_str(&mut hasher, b'L', &state.language.to_string());
    write_opt_str(
        &mut hasher,
        b'B',
        chunk.base.as_ref().map(|version| version.code.as_str()),
    );
    write_opt_str(
        &mut hasher,
        b'O',
        chunk.ours.as_ref().map(|version| version.code.as_str()),
    );
    write_opt_str(
        &mut hasher,
        b'T',
        chunk.theirs.as_ref().map(|version| version.code.as_str()),
    );

    let signatures: Vec<_> = state.signatures_for_chunk(chunk_index).collect();
    write_len(&mut hasher, b'S', signatures.len());
    for signature in signatures {
        write_str(&mut hasher, b'I', &signature.item);
        write_opt_str(&mut hasher, b'U', signature.signature.as_deref());
        write_opt_str(&mut hasher, b'b', signature.base.as_deref());
        write_opt_str(&mut hasher, b'o', signature.ours.as_deref());
        write_opt_str(&mut hasher, b't', signature.theirs.as_deref());
    }

    hasher.finalize().into()
}

fn write_len(hasher: &mut Sha256, tag: u8, len: usize) {
    hasher.update([tag]);
    hasher.update((len as u64).to_le_bytes());
}

fn write_bytes(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    write_len(hasher, tag, bytes.len());
    hasher.update(bytes);
}

fn write_str(hasher: &mut Sha256, tag: u8, text: &str) {
    write_bytes(hasher, tag, text.as_bytes());
}

fn write_opt_str(hasher: &mut Sha256, tag: u8, text: Option<&str>) {
    match text {
        Some(text) => {
            hasher.update([tag, 1]);
            write_bytes(hasher, tag, text.as_bytes());
        }
        None => hasher.update([tag, 0]),
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic::{ChunkState, Language, SignatureState, VersionState};
    use tempfile::tempdir;

    fn sample_state(
        path: &str,
        line_offset: usize,
        ours_code: &str,
        theirs_code: &str,
    ) -> SemanticState {
        SemanticState {
            file: path.to_owned(),
            language: Language::Rust,
            chunks: vec![ChunkState {
                items: vec!["fn alpha".to_owned()],
                summary: "summary".to_owned(),
                base: Some(VersionState {
                    lines: 1 + line_offset..=3 + line_offset,
                    code: "fn alpha() -> u32 {\n    1\n}".to_owned(),
                    changes: None,
                }),
                ours: Some(VersionState {
                    lines: 1 + line_offset..=3 + line_offset,
                    code: ours_code.to_owned(),
                    changes: None,
                }),
                theirs: Some(VersionState {
                    lines: 1 + line_offset..=3 + line_offset,
                    code: theirs_code.to_owned(),
                    changes: None,
                }),
            }],
            signatures: vec![SignatureState {
                item: "struct User".to_owned(),
                signature: Some("struct User {\n    id: u64,\n}".to_owned()),
                base: None,
                ours: None,
                theirs: None,
                chunks: vec![0],
            }],
        }
    }

    #[test]
    fn key_ignores_path_and_lines_but_distinguishes_sides_models_and_signatures() {
        let base_state = sample_state("src/a.rs", 0, "fn alpha() { 10 }", "fn alpha() { 20 }");
        let moved_state = sample_state("src/b.rs", 50, "fn alpha() { 10 }", "fn alpha() { 20 }");
        let swapped_state = sample_state("src/a.rs", 0, "fn alpha() { 20 }", "fn alpha() { 10 }");
        let mut changed_sig = base_state.clone();
        changed_sig.signatures[0].signature = Some("struct User {\n    id: i64,\n}".to_owned());

        let key = chunk_key("jev:jev-latest:v1", &base_state, 0);
        assert_eq!(key, chunk_key("jev:jev-latest:v1", &moved_state, 0));
        assert_ne!(key, chunk_key("jev:jev-latest:v1", &swapped_state, 0));
        assert_ne!(key, chunk_key("jev:jev-1.13.0:v1", &base_state, 0));
        assert_ne!(key, chunk_key("ollama:kev:0.8b-bf16:v1", &base_state, 0));
        assert_ne!(key, chunk_key("jev:jev-latest:v1", &changed_sig, 0));
    }

    #[test]
    fn stores_decisions_and_expires_entries_after_thirty_days() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("git-weld/cache");
        let cache = Cache::open(&db_path).unwrap();
        let state = sample_state("src/lib.rs", 0, "fn alpha() { 10 }", "fn alpha() { 20 }");
        let key_a = chunk_key("jev:jev-latest:v1", &state, 0);
        let key_b = chunk_key("jev:jev-1.13.0:v1", &state, 0);
        let decision = Decision::new(
            MergeAction::TakeOurs,
            Confidence::new(0.98).unwrap(),
            "jev-1.13.0 chose TakeOurs",
        );

        let t0 = 1_700_000_000;
        cache.put_at(&key_a, &decision, t0);
        assert_eq!(cache.get_at(&key_a, t0 + 60), Some(decision.clone()));
        assert_eq!(cache.get_at(&key_a, t0 + TTL_SECONDS), None);

        // Writing a new entry after `key_a` expires prunes `key_a`.
        cache.put_at(&key_b, &decision, t0 + TTL_SECONDS + 1);
        assert_eq!(cache.get_at(&key_a, t0 + 1), None);
        assert_eq!(cache.get_at(&key_b, t0 + TTL_SECONDS + 2), Some(decision));
    }

    #[test]
    fn corrupt_or_unwritable_cache_returns_none() {
        let dir = tempdir().unwrap();
        let bad_path = dir.path().join("not-sqlite");
        fs::write(&bad_path, b"not a sqlite database").unwrap();

        assert!(Cache::open(&bad_path).is_none());
        assert!(locate(dir.path()).is_none());
    }
}
