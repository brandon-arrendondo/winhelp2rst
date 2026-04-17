use std::path::PathBuf;

use clap::Parser;
use miette::{IntoDiagnostic, WrapErr};

mod rst;

/// Convert Windows WinHelp (.hlp) files to reStructuredText.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the input .hlp file.
    input: PathBuf,

    /// Output directory for generated .rst files and images.
    output_dir: PathBuf,

    /// Print detailed progress (one line per topic written).
    #[arg(short, long)]
    verbose: bool,

    /// Parse and validate the input without writing any output files.
    #[arg(long)]
    dry_run: bool,

    /// Force a WinHelp format version instead of trusting the |SYSTEM
    /// record.  Useful when the header misreports its version.
    #[arg(long, value_enum)]
    format_version: Option<FormatVersion>,
}

/// WinHelp format versions exposed by `--format-version`.
///
/// Each variant maps to the `minor_version` byte the parser uses to drive
/// LZ77 usage, topic-block size, and the pre-3.1 record-extraction variant.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum FormatVersion {
    #[value(name = "3.0")]
    V30,
    #[value(name = "3.1")]
    V31,
    #[value(name = "4.0")]
    V40,
}

impl FormatVersion {
    /// The `SystemInfo::minor_version` value that represents this format.
    fn minor_version(self) -> u16 {
        match self {
            FormatVersion::V30 => 15,
            FormatVersion::V31 => 21,
            FormatVersion::V40 => 27,
        }
    }

    fn label(self) -> &'static str {
        match self {
            FormatVersion::V30 => "3.0",
            FormatVersion::V31 => "3.1",
            FormatVersion::V40 => "4.0",
        }
    }
}

fn main() -> miette::Result<()> {
    let cli = Cli::parse();

    let mut opts = winhelp::ParseOptions::default();
    if let Some(fv) = cli.format_version {
        opts.format_version_override = Some(fv.minor_version());
        eprintln!("Forcing WinHelp {} parsing mode", fv.label());
    }

    eprintln!("Parsing {}...", cli.input.display());

    let helpfile = winhelp::HelpFile::from_path_with_options(&cli.input, &opts)
        .into_diagnostic()
        .wrap_err_with(|| format!("failed to parse {}", cli.input.display()))?;

    eprintln!(
        "Parsed '{}': {} topics, {} images",
        helpfile.title,
        helpfile.topics.len(),
        helpfile.images.len(),
    );

    if cli.dry_run {
        eprintln!("Dry run — no output files written.");
        return Ok(());
    }

    let mut verbose_cb = |i: usize, n: usize, id: &str| {
        eprintln!("  [{i}/{n}] {id}");
    };
    let progress: rst::TopicProgress<'_> = if cli.verbose {
        Some(&mut verbose_cb)
    } else {
        None
    };

    let summary = rst::write_all_with_progress(&helpfile, &cli.output_dir, progress)?;

    eprintln!(
        "Wrote {} .rst files ({} aliases) and {} images to {}",
        summary.topics_written,
        summary.aliases_written,
        summary.images_written,
        cli.output_dir.display(),
    );

    Ok(())
}
