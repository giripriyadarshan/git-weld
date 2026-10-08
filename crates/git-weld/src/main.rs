use clap::{Parser, builder::NonEmptyStringValueParser};
use git_weld::merge::write_manual_conflict;
use std::{path::PathBuf, process::ExitCode};

// Path values may start with '-' (e.g. a top-level file named `-lib.rs` in %P),
// so they are never parsed as flags.
#[derive(Parser)]
#[command(
    version,
    about = "Semantic Git merge driver (Phase 1: manual-conflict fallback)"
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
        default_value = "manual",
        value_parser = NonEmptyStringValueParser::new(),
        help = "Requested provider; model evaluation is not implemented yet"
    )]
    provider: String,

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

    match write_manual_conflict(&cli.base, &cli.ours, &cli.theirs, cli.marker_size) {
        Ok(()) => {
            eprintln!(
                "git-weld: manual merge required for {} \
                 (requested provider: {}; model evaluation is not implemented)",
                cli.path.display(),
                cli.provider
            );
            eprintln!(
                "git-weld: base={}, ours={}, theirs={}",
                cli.base.display(),
                cli.ours.display(),
                cli.theirs.display()
            );
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("git-weld: {error:#}");
            ExitCode::from(2)
        }
    }
}
