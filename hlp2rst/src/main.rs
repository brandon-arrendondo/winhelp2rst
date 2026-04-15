use std::path::PathBuf;

use clap::Parser;

mod rst;

/// Convert Windows WinHelp (.hlp) files to reStructuredText.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the input .hlp file.
    input: PathBuf,

    /// Output directory for generated .rst files and images.
    output_dir: PathBuf,
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    let helpfile = winhelp::HelpFile::from_path(&cli.input).map_err(|e| miette::miette!("{e}"))?;

    eprintln!(
        "Parsed '{}': {} topics",
        helpfile.title,
        helpfile.topics.len(),
    );

    rst::write_all(&helpfile, &cli.output_dir)?;

    eprintln!(
        "Wrote {} topic files to {}",
        helpfile.topics.iter().filter(|t| !t.id.is_empty()).count(),
        cli.output_dir.display(),
    );

    Ok(())
}
