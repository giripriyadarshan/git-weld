use clap::{Parser, builder::NonEmptyStringValueParser};
use git_weld::{
    ast::{self, Analysis},
    merge::{Versions, write_manual_conflict},
    provider::{self, AUTO_RESOLVE_THRESHOLD, Decision, MergeAction, SystemOneProvider},
    state::SemanticState,
};
use std::{
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
    about = "Semantic Git merge driver (Phase 3: manual fallback with advisory model decisions)"
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
        help = "System One provider: manual (no model), jev, or mock (offline tests)"
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

    let written = Versions::read(&cli.base, &cli.ours, &cli.theirs).and_then(|versions| {
        write_manual_conflict(&cli.ours, &versions, cli.marker_size)?;
        Ok(versions)
    });
    let versions = match written {
        Ok(versions) => versions,
        Err(error) => {
            eprintln!("git-weld: {error:#}");
            return ExitCode::from(2);
        }
    };

    eprintln!(
        "git-weld: manual merge required for {} (provider: {})",
        cli.path.display(),
        cli.provider
    );
    eprintln!(
        "git-weld: base={}, ours={}, theirs={}",
        cli.base.display(),
        cli.ours.display(),
        cli.theirs.display()
    );
    let provider = load_provider(&cli.provider);
    if let Some(analysis) = report_conflicting_items(&cli.path, &versions)
        && !analysis.conflicts.is_empty()
        && let Some(provider) = provider.as_deref()
    {
        report_decisions(provider, &cli.path, &analysis, &versions, cli.timeout);
    }
    ExitCode::from(1)
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

// A provider that cannot be loaded only costs the advisory decisions; the
// conflict is already written.
fn load_provider(name: &str) -> Option<Box<dyn SystemOneProvider>> {
    match provider::load(name, &|key| std::env::var(key).ok()) {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("git-weld: {error}; no model will be consulted");
            None
        }
    }
}

// The conflict is already written, so the analysis only adds diagnostics and
// can never change the merge result or exit code.
fn report_conflicting_items(path: &Path, versions: &Versions) -> Option<Analysis> {
    let analysis = panic::catch_unwind(|| {
        ast::analyze(path, &versions.base, &versions.ours, &versions.theirs)
    });
    match analysis {
        Ok(Ok(analysis)) => {
            let language = analysis.language;
            match analysis.conflicts.len() {
                0 => eprintln!("git-weld: no {language} item changed on both sides"),
                1 => eprintln!("git-weld: 1 {language} item changed on both sides:"),
                count => eprintln!("git-weld: {count} {language} items changed on both sides:"),
            }
            for conflict in &analysis.conflicts {
                eprintln!("git-weld:   {conflict}");
            }
            Some(analysis)
        }
        Ok(Err(error)) => {
            eprintln!("git-weld: semantic analysis skipped: {error}");
            None
        }
        Err(_) => {
            eprintln!("git-weld: semantic analysis failed unexpectedly");
            None
        }
    }
}

// Decisions are advisory until automatic resolution exists: they are
// reported, but the written conflict and the exit code stay the same.
fn report_decisions(
    provider: &dyn SystemOneProvider,
    path: &Path,
    analysis: &Analysis,
    versions: &Versions,
    timeout: Duration,
) {
    let name = provider.name();
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("git-weld: {name} evaluation skipped: cannot start a runtime: {error}");
            return;
        }
    };
    let evaluation = panic::catch_unwind(AssertUnwindSafe(|| {
        let state = SemanticState::new(path, analysis, versions);
        runtime.block_on(provider::evaluate(provider, &state, timeout))
    }));

    match evaluation {
        Ok(Ok(decisions)) => {
            match decisions.len() {
                1 => eprintln!("git-weld: {name} decided 1 item:"),
                count => eprintln!("git-weld: {name} decided {count} items:"),
            }
            for (conflict, decision) in analysis.conflicts.iter().zip(&decisions) {
                eprintln!("git-weld:   {}: {}", conflict.item, describe(decision));
            }
            if decisions.iter().any(Decision::is_auto_resolvable) {
                eprintln!(
                    "git-weld: automatic resolution is not implemented yet; \
                     the conflict is left for manual resolution"
                );
            }
        }
        Ok(Err(error)) => eprintln!("git-weld: {name} evaluation failed: {error}"),
        Err(_) => eprintln!("git-weld: {name} evaluation failed unexpectedly"),
    }
}

fn describe(decision: &Decision) -> String {
    let mut text = format!("{}, confidence {}", decision.decision, decision.confidence);
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
