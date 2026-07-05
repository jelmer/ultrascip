//! ultrascip
//!
//! Generate one SCIP index per detected build system for a Debian source
//! package. This is what `ogni scip --output-all` does; keeping it as a
//! separate binary lets the codegraph process pipeline evolve independently of
//! the ognibuild CLI.
//!
//! The per-language dispatch (build-system detection, on-demand indexer
//! install, release-binary download, BuildFixer retries) is inlined from
//! `ognibuild/src/actions/scip.rs` into the [`scip`] module; the rest
//! (installer resolution, session backends, etc.) is used as a library.

// Copied verbatim from ognibuild/src/actions/scip.rs; the only edits are the
// `crate::` -> `ognibuild::` path rewrites at the top. Kept diffable against
// upstream so future ognibuild changes to the SCIP orchestrator can be
// re-applied by re-copying and re-running the same sed. `run_scip` (the
// single-file variant) is unused here -- we only call `run_scip_multi` -- but
// stays put so the file stays a straight copy.
#[allow(dead_code)]
mod scip;

use clap::Parser;
use ognibuild::analyze::AnalyzedError;
use ognibuild::buildsystem::{detect_buildsystems, Error};
use ognibuild::fix_build::BuildFixer;
use ognibuild::installer::{
    auto_installation_scope, auto_installer, Error as InstallerError, Installer,
};
use ognibuild::session::{resolve_session_kind, Session, SessionKind};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    about = "Generate SCIP indexes for a Debian source package, one per detected build system."
)]
struct Args {
    /// Source directory to index.
    #[arg(long, short, default_value = ".")]
    directory: PathBuf,

    /// Directory to write one SCIP index file per build system into, each
    /// named after the indexed language (e.g. python.scip). Created if it does
    /// not exist.
    #[arg(long)]
    output_all: PathBuf,

    /// Session backend, e.g. "plain" or "unshare:sid".
    #[arg(long)]
    session: String,

    /// Before indexing, install the Debian source package's Build-Depends from
    /// debian/control via apt. These resolve to concrete packages, unlike the
    /// build system's own declared dependencies, so indexers that need the
    /// build environment present (e.g. scip-python reading setup.py metadata)
    /// work.
    #[arg(long)]
    apt_build_deps: bool,

    /// Isolate the session from the network while indexing. Off by default:
    /// most indexers need the network at some point (installing build deps,
    /// downloading release binaries, resolving package registries).
    #[arg(long)]
    offline: bool,

    /// Print verbose output.
    #[arg(long)]
    debug: bool,
}

fn main() -> std::process::ExitCode {
    let args = Args::parse();

    env_logger::builder()
        .format(|buf, record| {
            use std::io::Write;
            writeln!(buf, "{}", record.args())
        })
        .filter(
            None,
            if args.debug {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            },
        )
        .init();

    match run(&args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(RunError::NoBuildSystem) => {
            log::info!("No build systems detected in {}", args.directory.display());
            std::process::ExitCode::from(1)
        }
        Err(RunError::Ognibuild(Error::NoBuildSystemDetected)) => {
            // run_scip_multi returns this when none of the detected build
            // systems had a SCIP indexer. Distinct from NoBuildSystem above:
            // there we found nothing at all, here we found something we can't
            // index (e.g. a pear/composer project).
            log::info!("No SCIP indexer for any detected build system");
            std::process::ExitCode::from(1)
        }
        Err(RunError::Ognibuild(Error::Error(AnalyzedError::Detailed { error, .. }))) => {
            log::error!("Detailed error: {}", error);
            std::process::ExitCode::from(1)
        }
        Err(RunError::Ognibuild(e)) => {
            log::error!("{}", e);
            std::process::ExitCode::from(1)
        }
        Err(RunError::Setup(msg)) => {
            log::error!("{}", msg);
            std::process::ExitCode::from(1)
        }
    }
}

enum RunError {
    NoBuildSystem,
    Ognibuild(Error),
    Setup(String),
}

impl From<Error> for RunError {
    fn from(e: Error) -> Self {
        RunError::Ognibuild(e)
    }
}

fn run(args: &Args) -> Result<(), RunError> {
    let session_kind: SessionKind = args
        .session
        .parse()
        .map_err(|e: String| RunError::Setup(format!("--session: {}", e)))?;
    let session_kind = resolve_session_kind(Some(session_kind), None)
        .map_err(|e| RunError::Setup(format!("--session: {}", e)))?;

    let mut session: Box<dyn Session> = session_kind
        .build(Some("ultrascip"))
        .map_err(|e| RunError::Setup(format!("failed to set up session: {}", e)))?;
    session.set_isolate_network(args.offline);

    // Prepare the working directory inside the session. For unshare, this
    // copies the sources into the session's Debian root; for plain, it points
    // to the host path. project_from_directory returns the pair of paths.
    let project = session
        .project_from_directory(&args.directory, None)
        .map_err(|e| {
            RunError::Setup(format!(
                "failed to prepare directory {}: {}",
                args.directory.display(),
                e
            ))
        })?;
    session
        .chdir(project.internal_path())
        .map_err(|e| RunError::Setup(format!("chdir failed: {}", e)))?;
    std::env::set_current_dir(project.external_path()).map_err(|e| {
        RunError::Setup(format!(
            "cannot cd to {}: {}",
            project.external_path().display(),
            e
        ))
    })?;

    let bss = detect_buildsystems(project.external_path());
    if bss.is_empty() {
        return Err(RunError::NoBuildSystem);
    }

    // Match ogni scip's installer/scope defaults.
    let scope = auto_installation_scope(session.as_ref());
    let installer: Box<dyn Installer> = auto_installer(session.as_ref(), scope, None);

    // The InstallFixer is what turns "missing scip-python on PATH" into
    // "install scip-python via npm and retry". Without it, indexers fail on
    // the first missing tool.
    let install_fixer = ognibuild::fixers::InstallFixer::new(installer.as_ref(), scope);
    let fixers: Vec<&dyn BuildFixer<InstallerError>> = vec![&install_fixer];

    // With --apt-build-deps, the Debian source package's Build-Depends are the
    // authoritative set, so we skip installing the build systems' own declared
    // dependencies (redundant, and can pull in irrelevant tooling that fails
    // for unrelated reasons -- e.g. a Node build pulling in puppeteer).
    if args.apt_build_deps {
        install_apt_build_deps(session.as_ref(), project.external_path())?;
    }

    std::fs::create_dir_all(&args.output_all).map_err(|e| {
        RunError::Setup(format!(
            "cannot create {}: {}",
            args.output_all.display(),
            e
        ))
    })?;

    let bss_refs: Vec<&dyn ognibuild::buildsystem::BuildSystem> =
        bss.iter().map(|bs| bs.as_ref()).collect();
    scip::run_scip_multi(
        session.as_ref(),
        &bss_refs,
        installer.as_ref(),
        &fixers,
        &args.output_all,
    )?;
    Ok(())
}

/// Install the Debian source package's Build-Depends inside the session, from
/// its debian/control. Same helper ogni.rs uses for `--apt-build-deps`; kept
/// local so we don't depend on ogni's private API.
fn install_apt_build_deps(session: &dyn Session, external_dir: &Path) -> Result<(), Error> {
    let control = external_dir.join("debian/control");
    if !control.exists() {
        log::info!("--apt-build-deps given but no debian/control found; skipping");
        return Ok(());
    }
    log::info!("Installing Debian Build-Depends from {}", control.display());
    ognibuild::debian::satisfy_build_deps_from_control(session, &control)
        .map_err(|e| Error::Other(e.to_string()))
}
