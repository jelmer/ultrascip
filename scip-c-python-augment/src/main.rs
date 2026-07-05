use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use scip_c_python_augment::{augment_file, AugmentOptions, AugmentOutcome};

#[derive(Parser, Debug)]
#[command(about = "Augment a C SCIP index with Python symbols from CPython C extensions.")]
struct Args {
    /// Input C SCIP index (index.scip produced by scip-clang).
    input: PathBuf,

    /// Output SCIP index path.
    output: PathBuf,

    /// Source tree to scan for `.c` files. Defaults to the input index's
    /// `project_root` (or the current directory if none is set).
    #[arg(long)]
    source_root: Option<PathBuf>,

    /// Python distribution name (e.g. "dulwich"). Falls back to
    /// `[project].name` in pyproject.toml.
    #[arg(long)]
    python_package: Option<String>,

    /// Python distribution version. Defaults to the version of the first
    /// package seen in the input SCIP, or "0.0.0".
    #[arg(long)]
    python_version: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = AugmentOptions {
        source_root: args.source_root,
        python_package: args.python_package,
        python_version: args.python_version,
    };
    match augment_file(&args.input, &args.output, &opts)? {
        AugmentOutcome::Written(stats) => eprintln!(
            "wrote {} ({} documents, {} exports)",
            args.output.display(),
            stats.documents,
            stats.exports,
        ),
        AugmentOutcome::NoExports => {
            eprintln!(
                "no CPython extension exports found or no python package name; nothing to write"
            );
        }
    }
    Ok(())
}
