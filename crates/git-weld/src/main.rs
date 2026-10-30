use clap::{Parser, builder::NonEmptyStringValueParser};
use git_weld::{
    cache::Cache,
    log::{self, Lines, Record},
    merge::{
        Conflicts, LineMerge, Rendered, Version, Versions, write_manual_conflict, write_output,
    },
    provider::{self, AUTO_RESOLVE_THRESHOLD, Decision, MergeAction, SystemOneProvider},
    semantic::{Chunk, DivergedItem, Divergence, MAX_CLEAN_ANALYZE_BYTES, SemanticMerge},
};
use std::{
    ops::RangeInclusive,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

// Path values may start with '-' (e.g. a top-level file named `-lib.rs` in %P),
// so they are never parsed as flags.
#[derive(Parser)]
#[command(
    version,
    about = "Semantic Git merge driver: merges by lines, then lets a System One model \
             resolve each remaining conflict chunk by chunk"
)]
struct Cli {
    #[arg(
        short = 'o',
        long = "base",
        value_name = "FILE",
        allow_hyphen_values = true,
        help = "Common ancestor (%O)"
    )]
    base: PathBuf,

    #[arg(
        short = 'a',
        long,
        value_name = "FILE",
        allow_hyphen_values = true,
        help = "Our version and output file (%A)"
    )]
    ours: PathBuf,

    #[arg(
        short = 'b',
        long,
        value_name = "FILE",
        allow_hyphen_values = true,
        help = "Their version (%B)"
    )]
    theirs: PathBuf,

    #[arg(
        short = 'p',
        long,
        value_name = "PATH",
        allow_hyphen_values = true,
        help = "Logical working-tree path (%P)"
    )]
    path: PathBuf,

    #[arg(
        long,
        env = "WELD_PROVIDER",
        default_value = provider::MANUAL,
        value_parser = NonEmptyStringValueParser::new(),
        help = "System One provider: manual (no model), jev, ollama, or mock (offline tests)"
    )]
    provider: String,

    #[arg(
        long,
        env = "WELD_TIMEOUT",
        value_name = "SECONDS",
        default_value = "10",
        value_parser = parse_timeout,
        help = "Time limit for the provider's answer"
    )]
    timeout: Duration,

    #[arg(
        long,
        env = "WELD_NO_CACHE",
        value_parser = parse_no_cache,
        help = "Bypass the decision cache in .git/git-weld/cache"
    )]
    no_cache: bool,

    #[arg(
        short = 'l',
        long,
        value_name = "LENGTH",
        default_value_t = 7,
        value_parser = clap::value_parser!(u16).range(1..),
        help = "Conflict marker length (%L)"
    )]
    marker_size: u16,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("git-weld: {error:#}");
            ExitCode::from(2)
        }
    }
}

// Exits 0 when no conflict markers are left, 1 when some are, and 2 when the
// merge could not be written (`%A` is then left untouched).
fn run(cli: &Cli) -> anyhow::Result<ExitCode> {
    let versions = Versions::read(&cli.base, &cli.ours, &cli.theirs)?;
    let merge = match LineMerge::new(&versions) {
        Ok(merge) => merge,
        Err(error) => {
            eprintln!(
                "git-weld: {error}; {} is left as a single conflict",
                cli.path.display()
            );
            write_manual_conflict(&cli.ours, &versions, cli.marker_size)?;
            return Ok(ExitCode::from(1));
        }
    };

    if merge.is_clean() {
        let rendered = merge.render(&[], Conflicts::Markers(cli.marker_size));
        write_output(&cli.ours, &rendered.text)?;
        if should_analyze_clean(&versions) {
            let semantic = SemanticMerge::analyze(&cli.path, &merge);
            note_combined_items(semantic.combined_items());
        }
        return Ok(ExitCode::SUCCESS);
    }

    let semantic = SemanticMerge::analyze(&cli.path, &merge);
    report_chunks(cli, &merge, &semantic);

    let decisions = decide(cli, &semantic);
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
    let (rendered, syntax) = semantic.resolve(&mut takes, cli.marker_size);
    if let Err(error) = syntax {
        eprintln!(
            "git-weld: the stitched result does not parse ({error}), \
             so every chunk keeps its conflict markers"
        );
    }

    write_output(&cli.ours, &rendered.text)?;
    report_outcome(
        semantic.chunks(),
        &takes,
        &rendered,
        semantic.language().is_some(),
    );
    if let Some(decisions) = &decisions {
        write_log(cli, semantic.chunks(), decisions, &takes, &rendered);
    }
    note_combined_items(semantic.combined_items());
    Ok(if rendered.markers == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let seconds: f64 = value
        .trim()
        .parse()
        .map_err(|_| format!("`{value}` is not a number of seconds"))?;
    if !(seconds > 0.0 && seconds <= 3600.0) {
        return Err("the timeout must be more than 0 and at most 3600 seconds".to_owned());
    }
    Ok(Duration::from_secs_f64(seconds))
}

fn parse_no_cache(value: &str) -> Result<bool, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => Err(format!("`{value}` is not a valid boolean")),
    }
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

fn report_chunks(cli: &Cli, merge: &LineMerge, semantic: &SemanticMerge) {
    let conflicts = plural(merge.conflicts(), "conflict");
    let path = cli.path.display();
    if let Some(reason) = semantic.analysis_error() {
        eprintln!("git-weld: {path}: {conflicts} (provider: {})", cli.provider);
        eprintln!("git-weld: semantic analysis skipped: {reason}");
        return;
    }
    let chunks = semantic.chunks();
    eprintln!(
        "git-weld: {path}: {conflicts} in {} (provider: {}):",
        plural(chunks.len(), "chunk"),
        cli.provider
    );
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.decidable {
            eprintln!(
                "git-weld:   chunk {}: {}",
                index + 1,
                chunk.item_paths().join(", ")
            );
        } else {
            eprintln!(
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
fn decide(cli: &Cli, semantic: &SemanticMerge) -> Option<Decisions> {
    if !semantic.chunks().iter().any(|chunk| chunk.decidable) {
        return None;
    }
    let provider = load_provider(&cli.provider)?;
    let name = provider.name().to_owned();
    let (asked, state) = match semantic.semantic_state() {
        Ok(Some(state)) => state,
        Ok(None) => return None,
        Err(_) => {
            eprintln!("git-weld: {name} evaluation failed unexpectedly");
            return None;
        }
    };

    let cache = (!cli.no_cache && provider.cache_key().is_some())
        .then(|| Cache::open_in_repo(Path::new(".")))
        .flatten();
    let evaluation =
        provider::evaluate_with_cache(provider.as_ref(), &state, cache.as_ref(), cli.timeout);
    if let Some(error) = &evaluation.error {
        eprintln!("git-weld: {error}");
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

    eprintln!(
        "git-weld: {name} decided {}:",
        plural(entries.len(), "chunk")
    );
    for entry in &entries {
        eprintln!(
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
fn load_provider(name: &str) -> Option<Box<dyn SystemOneProvider>> {
    match provider::load(name, &|key| std::env::var(key).ok()) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("git-weld: {error}; no model will be consulted");
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
        eprintln!(
            "git-weld: resolved {}; no conflict markers are left",
            if chunks.len() == 1 {
                "the only chunk".to_owned()
            } else {
                format!("all {} chunks", chunks.len())
            }
        );
    } else if resolved == 0 {
        eprintln!(
            "git-weld: {} left for manual resolution, at {}",
            plural(left.len(), noun),
            line_list(&left)
        );
    } else {
        eprintln!(
            "git-weld: resolved {resolved} of {} chunks; {} left for manual resolution, at {}",
            chunks.len(),
            left.len(),
            line_list(&left)
        );
    }
}

// Logging failures are reported but never change the merge result.
fn write_log(
    cli: &Cli,
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
                file: cli.path.display().to_string(),
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
    if let Err(error) = log::append(Path::new(log::FILE_NAME), &records) {
        eprintln!("git-weld: cannot write {}: {error}", log::FILE_NAME);
    }
}

// Items that both sides changed but the line merge combined (Q9 of the
// blueprint tracker) are never sent to a model. A note points them out,
// since combined edits can still clash in meaning.
fn note_combined_items(combined_items: &[DivergedItem]) {
    for diverged in combined_items {
        let change = match diverged.kind {
            Divergence::BothAdded => "added",
            _ => "changed",
        };
        eprintln!(
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
