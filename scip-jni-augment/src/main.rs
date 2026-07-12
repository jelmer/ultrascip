use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use scip_jni_augment::{augment_file, AugmentOptions, AugmentOutcome};

#[derive(Parser, Debug)]
#[command(about = "Augment a C/C++ SCIP index with Java symbols exposed through JNI.")]
struct Args {
    /// Input C/C++ SCIP index (index.scip produced by scip-clang).
    input: PathBuf,

    /// Output SCIP index path.
    output: PathBuf,

    /// Source tree to scan for C/C++ files. Defaults to the input index's
    /// `project_root` (or the current directory if none is set).
    #[arg(long)]
    source_root: Option<PathBuf>,

    /// Maven package name in scip-java's `maven/<groupId>/<artifactId>`
    /// form. Falls back to the coordinates in pom.xml, then to the
    /// unversioned-package placeholder.
    #[arg(long)]
    java_package: Option<String>,

    /// Maven package version. Falls back to pom.xml, then to the
    /// unversioned-package placeholder.
    #[arg(long)]
    java_version: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let opts = AugmentOptions {
        source_root: args.source_root,
        java_package: args.java_package,
        java_version: args.java_version,
    };
    match augment_file(&args.input, &args.output, &opts)? {
        AugmentOutcome::Written(stats) => eprintln!(
            "wrote {} ({} documents, {} exports)",
            args.output.display(),
            stats.documents,
            stats.exports,
        ),
        AugmentOutcome::NoExports => {
            eprintln!("no JNI exports found; nothing to write");
        }
    }
    Ok(())
}
