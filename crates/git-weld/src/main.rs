use clap::{Parser, builder::NonEmptyStringValueParser};
use git_weld::{DriverOptions, MergeOutcome, provider, run_driver};
use std::{
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
    let options = DriverOptions {
        base: &cli.base,
        ours: &cli.ours,
        theirs: &cli.theirs,
        path: &cli.path,
        provider: &cli.provider,
        timeout: cli.timeout,
        no_cache: cli.no_cache,
        marker_size: cli.marker_size,
    };
    match run_driver(
        &options,
        &|key| std::env::var(key).ok(),
        Path::new("."),
        &mut std::io::stderr(),
    ) {
        Ok(MergeOutcome::Resolved) => ExitCode::SUCCESS,
        Ok(MergeOutcome::ManualResolution) => ExitCode::from(1),
        Err(error) => {
            eprintln!("git-weld: {error:#}");
            ExitCode::from(2)
        }
    }
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
