use clap::{Parser, builder::NonEmptyStringValueParser};
use git_weld::{
    ast::{self, Analysis, Divergence},
    cache::{self, Cache},
    chunk::{self, Chunk},
    log::{self, Lines, Record},
    merge::{
        Conflicts, LineMerge, Rendered, Span, Version, Versions, write_manual_conflict,
        write_output,
    },
    provider::{self, AUTO_RESOLVE_THRESHOLD, Decision, MergeAction, SystemOneProvider},
    state::SemanticState,
};
use std::{
    ops::RangeInclusive,
    panic::{self, AssertUnwindSafe},
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

const MAX_CLEAN_ANALYZE_BYTES: usize = 512 * 1024;

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
        if should_analyze_clean(&versions)
            && let Ok(analysis) = analyze(&cli.path, &versions)
        {
            note_combined_items(&analysis, &merge, &[]);
        }
        return Ok(ExitCode::SUCCESS);
    }

    let analysis = analyze(&cli.path, &versions);
    let chunks = find_chunks(&merge, analysis.as_ref().ok());
    report_chunks(cli, &merge, &chunks, analysis.as_ref().err());
    let analysis = analysis.ok();

    let decisions = match &analysis {
        Some(analysis) => decide(cli, analysis, &merge, &chunks),
        None => None,
    };
    let mut takes: Vec<Option<Version>> = chunks
        .iter()
        .enumerate()
        .map(|(index, _)| {
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
    if let Some(analysis) = &analysis
        && takes.iter().any(Option::is_some)
        && let Err(error) = check_stitched(analysis, &merge, &chunks, &takes)
    {
        eprintln!(
            "git-weld: the stitched result does not parse ({error}), \
             so every chunk keeps its conflict markers"
        );
        takes.fill(None);
    }

    let rendered = merge.render(&spans(&chunks, &takes), Conflicts::Markers(cli.marker_size));
    write_output(&cli.ours, &rendered.text)?;
    report_outcome(&chunks, &takes, &rendered, analysis.is_some());
    if let Some(decisions) = &decisions {
        write_log(cli, &chunks, decisions, &takes, &rendered);
    }
    if let Some(analysis) = &analysis {
        note_combined_items(analysis, &merge, &chunks);
    }
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

// Without an analysis, conflicts keep the text merge's markers; nothing else
// changes.
fn analyze(path: &Path, versions: &Versions) -> Result<Analysis, String> {
    panic::catch_unwind(|| ast::analyze(path, &versions.base, &versions.ours, &versions.theirs))
        .map_err(|_| "it failed unexpectedly".to_owned())?
        .map_err(|error| error.to_string())
}

fn find_chunks(merge: &LineMerge, analysis: Option<&Analysis>) -> Vec<Chunk> {
    panic::catch_unwind(AssertUnwindSafe(|| chunk::chunks(merge, analysis))).unwrap_or_else(|_| {
        eprintln!("git-weld: chunking failed unexpectedly; no model will be consulted");
        chunk::chunks(merge, None)
    })
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn report_chunks(cli: &Cli, merge: &LineMerge, chunks: &[Chunk], skipped: Option<&String>) {
    let conflicts = plural(merge.conflicts(), "conflict");
    let path = cli.path.display();
    if let Some(reason) = skipped {
        eprintln!("git-weld: {path}: {conflicts} (provider: {})", cli.provider);
        eprintln!("git-weld: semantic analysis skipped: {reason}");
        return;
    }
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
fn decide(
    cli: &Cli,
    analysis: &Analysis,
    merge: &LineMerge,
    chunks: &[Chunk],
) -> Option<Decisions> {
    let asked: Vec<usize> = (0..chunks.len())
        .filter(|&index| chunks[index].decidable)
        .collect();
    if asked.is_empty() {
        return None;
    }
    let provider = load_provider(&cli.provider)?;
    let name = provider.name().to_owned();
    let Ok(state) = panic::catch_unwind(AssertUnwindSafe(|| {
        let selected: Vec<&Chunk> = asked.iter().map(|&index| &chunks[index]).collect();
        SemanticState::new(&cli.path, analysis, merge, &selected)
    })) else {
        eprintln!("git-weld: {name} evaluation failed unexpectedly");
        return None;
    };

    let cache_store = (!cli.no_cache)
        .then(|| provider.cache_key())
        .flatten()
        .and_then(|key| Cache::open_in_repo(Path::new(".")).map(|cache| (cache, key)));
    let keys: Vec<Option<[u8; 32]>> = (0..asked.len())
        .map(|position| {
            cache_store
                .as_ref()
                .map(|(_, provider_key)| cache::chunk_key(provider_key, &state, position))
        })
        .collect();

    let mut decided: Vec<Option<(Decision, bool)>> = vec![None; asked.len()];
    let mut misses: Vec<usize> = Vec::new();
    for position in 0..asked.len() {
        if let (Some((cache, _)), Some(key)) = (&cache_store, &keys[position])
            && let Some(hit) = cache.get(key)
        {
            decided[position] = Some((hit, true));
        } else {
            misses.push(position);
        }
    }

    if !misses.is_empty() {
        let missed_state = if misses.len() == asked.len() {
            state.clone()
        } else {
            state.select_chunks(&misses)
        };
        let fresh = evaluate_misses(&name, provider.as_ref(), &missed_state, cli.timeout);
        if let Some(fresh) = fresh {
            let trimmed = provider.would_trim(&missed_state);
            for (&position, decision) in misses.iter().zip(fresh) {
                if !trimmed && let (Some((cache, _)), Some(key)) = (&cache_store, &keys[position]) {
                    cache.put(key, &decision);
                }
                decided[position] = Some((decision, false));
            }
        }
    }

    let entries: Vec<DecidedChunk> = asked
        .into_iter()
        .zip(decided)
        .filter_map(|(chunk, entry)| {
            let (decision, cached) = entry?;
            Some(DecidedChunk {
                chunk,
                decision,
                cached,
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

fn evaluate_misses(
    name: &str,
    provider: &dyn SystemOneProvider,
    state: &SemanticState,
    timeout: Duration,
) -> Option<Vec<Decision>> {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("git-weld: {name} evaluation skipped: cannot start a runtime: {error}");
            return None;
        }
    };
    let evaluation = panic::catch_unwind(AssertUnwindSafe(|| {
        runtime.block_on(provider::evaluate(provider, state, timeout))
    }));
    match evaluation {
        Ok(Ok(decisions)) => Some(decisions),
        Ok(Err(error)) => {
            eprintln!("git-weld: {name} evaluation failed: {error}");
            None
        }
        Err(_) => {
            eprintln!("git-weld: {name} evaluation failed unexpectedly");
            None
        }
    }
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

fn spans(chunks: &[Chunk], takes: &[Option<Version>]) -> Vec<Span> {
    chunks
        .iter()
        .zip(takes)
        .map(|(chunk, &take)| Span {
            regions: chunk.regions.clone(),
            take,
        })
        .collect()
}

// The resolved chunks must fit the code around them. Conflicts that stay
// manual are filled with our side for the check, since markers never parse.
fn check_stitched(
    analysis: &Analysis,
    merge: &LineMerge,
    chunks: &[Chunk],
    takes: &[Option<Version>],
) -> Result<(), String> {
    let probe = merge.render(&spans(chunks, takes), Conflicts::Fill(Version::Ours));
    panic::catch_unwind(|| ast::check_syntax(analysis.language, &probe.text))
        .map_err(|_| "the parser failed unexpectedly".to_owned())?
        .map_err(|error| error.to_string())
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
                items: chunks[index].item_paths(),
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
fn note_combined_items(analysis: &Analysis, merge: &LineMerge, chunks: &[Chunk]) {
    for diverged in &analysis.diverged_items {
        let in_chunk = chunks.iter().any(|chunk| {
            [
                (Version::Base, &diverged.base),
                (Version::Ours, &diverged.ours),
                (Version::Theirs, &diverged.theirs),
            ]
            .into_iter()
            .any(|(version, location)| {
                location.as_ref().is_some_and(|location| {
                    let span = merge.bytes(version, chunk.lines(merge, version));
                    location.bytes.start < span.end && span.start < location.bytes.end
                })
            })
        });
        if !in_chunk {
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
