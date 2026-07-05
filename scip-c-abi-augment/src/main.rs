use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use scip_c_abi_augment::augment_file;

#[derive(Parser, Debug)]
#[command(
    about = "Augment a Rust SCIP index with C ABI symbols for #[no_mangle] / extern \"C\" exports."
)]
struct Args {
    /// Input Rust SCIP index (rust.scip produced by rust-analyzer).
    input: PathBuf,

    /// Output SCIP index path.
    output: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let stats = augment_file(&args.input, &args.output)?;
    eprintln!(
        "wrote {} ({} documents, {} exports, {} external symbols)",
        args.output.display(),
        stats.documents,
        stats.exports,
        stats.external_symbols,
    );
    Ok(())
}
