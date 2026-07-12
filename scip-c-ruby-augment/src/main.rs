use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use scip_c_ruby_augment::{augment_file, AugmentOptions, AugmentOutcome};

#[derive(Parser, Debug)]
#[command(about = "Augment a C/C++ SCIP index with Ruby symbols from Ruby C extensions.")]
struct Args {
    /// Input C/C++ SCIP index (index.scip produced by scip-clang).
    input: PathBuf,

    /// Output SCIP index path.
    output: PathBuf,

    /// Source tree to scan for C/C++ files. Defaults to the input index's
    /// `project_root` (or the current directory if none is set).
    #[arg(long)]
    source_root: Option<PathBuf>,

    /// Gem name (e.g. "nokogiri"). Falls back to the `name` attribute of a
    /// `*.gemspec` in the source root.
    #[arg(long)]
    ruby_gem: Option<String>,

    /// Gem version. Falls back to the gemspec's `version` attribute, then to
    /// the version of the first package seen in the input SCIP, or "0.0.0".
    #[arg(long)]
    ruby_version: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = AugmentOptions {
        source_root: args.source_root,
        ruby_gem: args.ruby_gem,
        ruby_version: args.ruby_version,
    };
    match augment_file(&args.input, &args.output, &opts)? {
        AugmentOutcome::Written(stats) => eprintln!(
            "wrote {} ({} documents, {} exports)",
            args.output.display(),
            stats.documents,
            stats.exports,
        ),
        AugmentOutcome::NoExports => {
            eprintln!("no Ruby C extension registrations found or no gem name; nothing to write");
        }
    }
    Ok(())
}
