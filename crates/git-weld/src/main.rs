use clap::{Parser, builder::NonEmptyStringValueParser};
use git_weld::{
    ast,
    merge::{Versions, write_manual_conflict},
};
use std::{
    panic,
    path::{Path, PathBuf},
    process::ExitCode,
};

// Path values may start with '-' (e.g. a top-level file named `-lib.rs` in %P),
// so they are never parsed as flags.
#[derive(Parser)]
#[command(
    version,
    about = "Semantic Git merge driver (Phase 2: manual fallback with AST conflict reports)"
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
    report_conflicting_items(&cli.path, &versions);
    ExitCode::from(1)
}

// The conflict is already written, so the analysis only adds diagnostics and
// can never change the merge result or exit code.
fn report_conflicting_items(path: &Path, versions: &Versions) {
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
        }
        Ok(Err(error)) => eprintln!("git-weld: semantic analysis skipped: {error}"),
        Err(_) => eprintln!("git-weld: semantic analysis failed unexpectedly"),
    }
}
