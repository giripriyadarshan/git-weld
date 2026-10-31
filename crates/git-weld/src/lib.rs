//! Semantic three-way Git merge driver library seam.
//!
//! [`run_driver`] executes the complete merge pipeline for a single file:
//! reading the three input [`Versions`], running [`LineMerge`] and
//! [`SemanticMerge`], consulting the SQLite [`Cache`] and configured
//! [`SystemOneProvider`], verifying stitched syntax, atomically writing `%A`,
//! and appending the `.git-weld.log` decision log.

pub mod cache;
mod log;
pub mod merge;
pub mod provider;
pub mod semantic;
pub mod state {
    pub use crate::semantic::{ChunkState, SemanticState, SignatureState, VersionState};
}

use cache::Cache;
use log::{Lines, Record};
use merge::{
    Conflicts, LineMerge, Rendered, Version, Versions, write_manual_conflict, write_output,
};
use provider::{AUTO_RESOLVE_THRESHOLD, Decision, MergeAction, SystemOneProvider};
use semantic::{Chunk, DivergedItem, Divergence, MAX_CLEAN_ANALYZE_BYTES, SemanticMerge};
use std::{io::Write, ops::RangeInclusive, path::Path, time::Duration};

/// Inputs and configuration for a single [`run_driver`] invocation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverOptions<'a> {
    pub base: &'a Path,
    pub ours: &'a Path,
    pub theirs: &'a Path,
    pub path: &'a Path,
    pub provider: &'a str,
    pub timeout: Duration,
    pub no_cache: bool,
    pub marker_size: u16,
}

/// Whether [`run_driver`] resolved every conflict in `%A` (`0`) or left
/// standard conflict markers for manual resolution (`1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    Resolved,
    ManualResolution,
}

/// Executes the three-way merge driver pipeline, reading provider settings
/// from `env`, resolving `.git-weld.log` and `<git-common-dir>/git-weld/cache`
/// relative to `work_dir`, and writing `git-weld: ...` notes to `diagnostics`.
pub fn run_driver(
    options: &DriverOptions<'_>,
    env: provider::Env<'_>,
    work_dir: &Path,
    diagnostics: &mut dyn Write,
) -> anyhow::Result<MergeOutcome> {
    let versions = Versions::read(options.base, options.ours, options.theirs)?;
    let merge = match LineMerge::new(&versions) {
        Ok(merge) => merge,
        Err(error) => {
            let _ = writeln!(
                diagnostics,
                "git-weld: {error}; {} is left as a single conflict",
                options.path.display()
            );
            write_manual_conflict(options.ours, &versions, options.marker_size)?;
            return Ok(MergeOutcome::ManualResolution);
        }
    };

    if merge.is_clean() {
        let rendered = merge.render(&[], Conflicts::Markers(options.marker_size));
        write_output(options.ours, &rendered.text)?;
        if should_analyze_clean(&versions) {
            let semantic = SemanticMerge::analyze(options.path, &merge);
            note_combined_items(diagnostics, semantic.combined_items());
        }
        return Ok(MergeOutcome::Resolved);
    }

    let semantic = SemanticMerge::analyze(options.path, &merge);
    report_chunks(diagnostics, options, &merge, &semantic);

    let decisions = decide(diagnostics, options, env, work_dir, &semantic);
    let mut takes: Vec<Option<Version>> = (0..semantic.chunks().len())
        .map(|index| {
            decisions
                .as_ref()
                .and_then(|decisions| decisions.decided(index))
                .filter(|decision| decision.is_auto_resolvable())
                .map(|decision| match decision.decision {
                    MergeAction::TakeOurs => Version::Ours,
                    _ => Version::Theirs,
                })
        })
        .collect();
    let (rendered, syntax) = semantic.resolve(&mut takes, options.marker_size);
    if let Err(error) = syntax {
        let _ = writeln!(
            diagnostics,
            "git-weld: the stitched result does not parse ({error}), \
             so every chunk keeps its conflict markers"
        );
    }

    write_output(options.ours, &rendered.text)?;
    report_outcome(
        diagnostics,
        semantic.chunks(),
        &takes,
        &rendered,
        semantic.language().is_some(),
    );
    if let Some(decisions) = &decisions {
        write_log(
            diagnostics,
            work_dir,
            options.path,
            semantic.chunks(),
            decisions,
            &takes,
            &rendered,
        );
    }
    note_combined_items(diagnostics, semantic.combined_items());
    Ok(if rendered.markers == 0 {
        MergeOutcome::Resolved
    } else {
        MergeOutcome::ManualResolution
    })
}

// A clean merge only needs AST analysis when both sides changed the file
// differently and the inputs are small enough to analyze quickly.
fn should_analyze_clean(versions: &Versions) -> bool {
    versions.base.len() <= MAX_CLEAN_ANALYZE_BYTES
        && versions.ours.len() <= MAX_CLEAN_ANALYZE_BYTES
        && versions.theirs.len() <= MAX_CLEAN_ANALYZE_BYTES
        && versions.base != versions.ours
        && versions.base != versions.theirs
        && versions.ours != versions.theirs
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn report_chunks(
    diagnostics: &mut dyn Write,
    options: &DriverOptions<'_>,
    merge: &LineMerge,
    semantic: &SemanticMerge,
) {
    let conflicts = plural(merge.conflicts(), "conflict");
    let path = options.path.display();
    if let Some(reason) = semantic.analysis_error() {
        let _ = writeln!(
            diagnostics,
            "git-weld: {path}: {conflicts} (provider: {})",
            options.provider
        );
        let _ = writeln!(diagnostics, "git-weld: semantic analysis skipped: {reason}");
        return;
    }
    let chunks = semantic.chunks();
    let _ = writeln!(
        diagnostics,
        "git-weld: {path}: {conflicts} in {} (provider: {}):",
        plural(chunks.len(), "chunk"),
        options.provider
    );
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.decidable {
            let _ = writeln!(
                diagnostics,
                "git-weld:   chunk {}: {}",
                index + 1,
                chunk.item_paths().join(", ")
            );
        } else {
            let _ = writeln!(
                diagnostics,
                "git-weld:   chunk {}: outside every item, so no model is asked",
                index + 1
            );
        }
    }
}

struct DecidedChunk {
    chunk: usize,
    decision: Decision,
    cached: bool,
}

// The decisions for the decidable chunks, in chunk order.
struct Decisions {
    provider: String,
    entries: Vec<DecidedChunk>,
}

impl Decisions {
    fn decided(&self, chunk: usize) -> Option<&Decision> {
        self.entries
            .iter()
            .find(|entry| entry.chunk == chunk)
            .map(|entry| &entry.decision)
    }
}

// Asks the provider about every decidable chunk not already in the cache, in
// one evaluation. Problems only cost the missed decisions; every undecided
// chunk then keeps its markers.
fn decide(
    diagnostics: &mut dyn Write,
    options: &DriverOptions<'_>,
    env: provider::Env<'_>,
    work_dir: &Path,
    semantic: &SemanticMerge,
) -> Option<Decisions> {
    if !semantic.chunks().iter().any(|chunk| chunk.decidable) {
        return None;
    }
    let provider = load_provider(diagnostics, options.provider, env)?;
    let name = provider.name().to_owned();
    let (asked, state) = match semantic.semantic_state() {
        Ok(Some(state)) => state,
        Ok(None) => return None,
        Err(_) => {
            let _ = writeln!(
                diagnostics,
                "git-weld: {name} evaluation failed unexpectedly"
            );
            return None;
        }
    };

    let cache = (!options.no_cache && provider.cache_key().is_some())
        .then(|| Cache::open_in_repo(work_dir))
        .flatten();
    let evaluation =
        provider::evaluate_with_cache(provider.as_ref(), &state, cache.as_ref(), options.timeout);
    if let Some(error) = &evaluation.error {
        let _ = writeln!(diagnostics, "git-weld: {error}");
    }

    let entries: Vec<DecidedChunk> = asked
        .into_iter()
        .zip(evaluation.decisions)
        .filter_map(|(chunk, resolved)| {
            let resolved = resolved?;
            Some(DecidedChunk {
                chunk,
                decision: resolved.decision,
                cached: resolved.cached,
            })
        })
        .collect();
    if entries.is_empty() {
        return None;
    }

    let _ = writeln!(
        diagnostics,
        "git-weld: {name} decided {}:",
        plural(entries.len(), "chunk")
    );
    for entry in &entries {
        let _ = writeln!(
            diagnostics,
            "git-weld:   chunk {}: {}",
            entry.chunk + 1,
            describe(&entry.decision, entry.cached)
        );
    }
    Some(Decisions {
        provider: name,
        entries,
    })
}

// A provider that cannot be loaded only costs the decisions.
fn load_provider(
    diagnostics: &mut dyn Write,
    name: &str,
    env: provider::Env<'_>,
) -> Option<Box<dyn SystemOneProvider>> {
    match provider::load(name, env) {
        Ok(provider) => provider,
        Err(error) => {
            let _ = writeln!(diagnostics, "git-weld: {error}; no model will be consulted");
            None
        }
    }
}

// Such as "line 4", "lines 1-9", or "lines 1-9, 11 and 13-18".
fn line_list(ranges: &[&RangeInclusive<usize>]) -> String {
    let parts: Vec<String> = ranges
        .iter()
        .map(|range| {
            if range.start() == range.end() {
                range.start().to_string()
            } else {
                format!("{}-{}", range.start(), range.end())
            }
        })
        .collect();
    let noun = match ranges {
        [only] if only.start() == only.end() => "line",
        _ => "lines",
    };
    match parts.as_slice() {
        [rest @ .., last] if !rest.is_empty() => format!("{noun} {} and {last}", rest.join(", ")),
        _ => format!("{noun} {}", parts.concat()),
    }
}

fn report_outcome(
    diagnostics: &mut dyn Write,
    chunks: &[Chunk],
    takes: &[Option<Version>],
    rendered: &Rendered,
    analyzed: bool,
) {
    let noun = if analyzed { "chunk" } else { "conflict" };
    let resolved = takes.iter().flatten().count();
    let left: Vec<&RangeInclusive<usize>> = takes
        .iter()
        .zip(&rendered.lines)
        .filter(|(take, _)| take.is_none())
        .filter_map(|(_, lines)| lines.as_ref())
        .collect();
    if left.is_empty() {
        let _ = writeln!(
            diagnostics,
            "git-weld: resolved {}; no conflict markers are left",
            if chunks.len() == 1 {
                "the only chunk".to_owned()
            } else {
                format!("all {} chunks", chunks.len())
            }
        );
    } else if resolved == 0 {
        let _ = writeln!(
            diagnostics,
            "git-weld: {} left for manual resolution, at {}",
            plural(left.len(), noun),
            line_list(&left)
        );
    } else {
        let _ = writeln!(
            diagnostics,
            "git-weld: resolved {resolved} of {} chunks; {} left for manual resolution, at {}",
            chunks.len(),
            left.len(),
            line_list(&left)
        );
    }
}

// Logging failures are reported but never change the merge result.
fn write_log(
    diagnostics: &mut dyn Write,
    work_dir: &Path,
    path: &Path,
    chunks: &[Chunk],
    decisions: &Decisions,
    takes: &[Option<Version>],
    rendered: &Rendered,
) {
    let time = log::now();
    let records: Vec<Record> = decisions
        .entries
        .iter()
        .map(|entry| {
            let index = entry.chunk;
            Record {
                time: time.clone(),
                file: path.display().to_string(),
                chunk: index + 1,
                items: chunks[index].items.clone(),
                lines: rendered.lines[index].clone().map(Lines::from),
                provider: decisions.provider.clone(),
                decision: entry.decision.decision,
                confidence: entry.decision.confidence.value(),
                reason: entry.decision.reason.clone(),
                resolved: takes[index].is_some(),
                cached: entry.cached,
            }
        })
        .collect();
    if let Err(error) = log::append(&work_dir.join(log::FILE_NAME), &records) {
        let _ = writeln!(
            diagnostics,
            "git-weld: cannot write {}: {error}",
            log::FILE_NAME
        );
    }
}

// Items that both sides changed but the line merge combined (Q9 of the
// blueprint tracker) are never sent to a model. A note points them out,
// since combined edits can still clash in meaning.
fn note_combined_items(diagnostics: &mut dyn Write, combined_items: &[DivergedItem]) {
    for diverged in combined_items {
        let change = match diverged.kind {
            Divergence::BothAdded => "added",
            _ => "changed",
        };
        let _ = writeln!(
            diagnostics,
            "git-weld: note: both sides {change} {}, but the edits do not overlap, \
             so they were merged line by line",
            diverged.item
        );
    }
}

fn describe(decision: &Decision, cached: bool) -> String {
    let source = if cached { " (cached)" } else { "" };
    let mut text = format!(
        "{}{source}, confidence {}",
        decision.decision, decision.confidence
    );
    if decision.decision != MergeAction::Manual {
        if decision.is_auto_resolvable() {
            text.push_str(&format!(", above the {AUTO_RESOLVE_THRESHOLD} threshold"));
        } else {
            text.push_str(&format!(
                ", not above the {AUTO_RESOLVE_THRESHOLD} threshold"
            ));
        }
    }
    let reason = decision
        .reason
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !reason.is_empty() {
        text.push_str(": ");
        text.push_str(&reason);
    }
    text
}
