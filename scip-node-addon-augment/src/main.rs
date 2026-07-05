use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use scip_node_addon_augment::{augment_file, AugmentOptions, AugmentOutcome};

#[derive(Parser, Debug)]
#[command(about = "Augment a C/C++ SCIP index with JS symbols from Node.js native addons.")]
struct Args {
    /// Input C/C++ SCIP index (index.scip produced by scip-clang).
    input: PathBuf,

    /// Output SCIP index path.
    output: PathBuf,

    /// Source tree to scan for `.c`/`.cc`/`.cpp`/`.cxx` files. Defaults to
    /// the input index's `project_root` (or the current directory if none
    /// is set).
    #[arg(long)]
    source_root: Option<PathBuf>,

    /// JS package name (e.g. "node-libpq"). Falls back to `package.json`
    /// `name` field.
    #[arg(long)]
    js_package: Option<String>,

    /// JS package version. Falls back to `package.json` `version`, then to
    /// the version of the first C SCIP symbol, then to "0.0.0".
    #[arg(long)]
    js_version: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = AugmentOptions {
        source_root: args.source_root,
        js_package: args.js_package,
        js_version: args.js_version,
    };
    match augment_file(&args.input, &args.output, &opts)? {
        AugmentOutcome::Written(stats) => eprintln!(
            "wrote {} ({} documents, {} exports)",
            args.output.display(),
            stats.documents,
            stats.exports,
        ),
        AugmentOutcome::NoExports => {
            eprintln!("no Node.js addon exports found or no JS package name; nothing to write");
        }
    }
    Ok(())
}
