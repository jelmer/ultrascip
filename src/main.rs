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

// Started as a copy of ognibuild/src/actions/scip.rs (with `crate::` ->
// `ognibuild::` path rewrites), but has since grown FFI companion indexes,
// the manifest report and all-features Rust indexing, so it is no longer
// diffable against upstream. `run_scip` (the single-file variant) is unused
// here -- we only call `run_scip_multi` -- but stays put for parity with
// upstream.
#[allow(dead_code)]
mod scip;

mod manifest;
mod version;

use clap::Parser;
use manifest::{IndexEntry, Manifest};
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

    /// Skip the debian-lsp pass that produces debian.scip from the Debian
    /// packaging files. Off by default; the pass is skipped anyway when the
    /// source tree has no debian/ subdirectory.
    #[arg(long)]
    no_debian_lsp: bool,

    /// Skip the makefile-lsp pass that produces makefile.scip from
    /// debian/rules and any Makefile / *.mk in the source tree. Off by
    /// default; the pass is skipped anyway when no such files are found.
    #[arg(long)]
    no_makefile_lsp: bool,

    /// Skip the scip-shell pass that produces shell.scip from shell scripts
    /// in the source tree. Off by default.
    #[arg(long)]
    no_shell: bool,

    /// Skip the scip-po pass that produces po.scip from GNU gettext .po/.pot
    /// files in the source tree. Off by default; the pass is skipped anyway
    /// when the tree has no .po/.pot files.
    #[arg(long)]
    no_po: bool,

    /// Skip the scip-tree-sitter pass that produces tree-sitter.scip for
    /// files no other indexer covered. Off by default.
    #[arg(long)]
    no_tree_sitter: bool,

    /// Package name to record in scip-shell's emitted symbols. Optional; when
    /// unset, scip-shell falls back to its default ("shell-project").
    #[arg(long)]
    package_name: Option<String>,

    /// Package version to record in scip-shell's emitted symbols. Optional;
    /// when unset, scip-shell falls back to its default ("0.0.0").
    #[arg(long)]
    package_version: Option<String>,

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
        Err(RunError::IndexersFailed) => {
            // The per-build-system failures were already logged as warnings
            // (and recorded in manifest.json); this only sets the exit code.
            log::error!("One or more SCIP indexers failed");
            std::process::ExitCode::from(1)
        }
    }
}

enum RunError {
    NoBuildSystem,
    Ognibuild(Error),
    Setup(String),
    /// Some build systems failed to index; details are in the manifest and
    /// the log.
    IndexersFailed,
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

    // The InstallFixer is what turns "missing scip-typescript on PATH" into
    // "install scip-typescript via npm and retry". Without it, indexers fail on
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
    // Keep the run_scip_multi error, if any, but still run the post-passes:
    // debian.scip and tree-sitter.scip stand on their own and remain useful
    // even if some language indexer failed. The saved error is returned at
    // the end when nothing else failed harder first.
    let indexer_result = scip::run_scip_multi(
        session.as_ref(),
        &bss_refs,
        installer.as_ref(),
        &fixers,
        &args.output_all,
    );

    let mut manifest = Manifest::new();
    let indexer_error = match indexer_result {
        Ok(report) => {
            manifest.indexes = report.indexes;
            manifest.failures = report.failures;
            None
        }
        Err(e) => Some(e),
    };

    let post_result = run_post_passes(args, project.external_path(), &mut manifest);

    // Summarize the per-index versions now that every pass has contributed.
    manifest.collect_generators();

    // Write the manifest even when a pass failed: it describes whatever
    // indexes did land in the output directory.
    let manifest_path = args.output_all.join("manifest.json");
    manifest
        .write(&manifest_path)
        .map_err(|e| RunError::Setup(format!("cannot write {}: {}", manifest_path.display(), e)))?;
    log::info!("Wrote manifest to {}", manifest_path.display());

    post_result?;
    if let Some(e) = indexer_error {
        return Err(e.into());
    }
    if !manifest.failures.is_empty() {
        return Err(RunError::IndexersFailed);
    }
    Ok(())
}

/// Run the host-side post-passes, recording each index written in the
/// manifest. Stops at the first failing pass.
fn run_post_passes(
    args: &Args,
    source_dir: &Path,
    manifest: &mut Manifest,
) -> Result<(), RunError> {
    if !args.no_debian_lsp {
        if let Some(entry) = run_debian_lsp(source_dir, &args.output_all)? {
            manifest.indexes.push(entry);
        }
    }
    if !args.no_makefile_lsp {
        if let Some(entry) = run_makefile_lsp(source_dir, &args.output_all)? {
            manifest.indexes.push(entry);
        }
    }
    if !args.no_shell {
        if let Some(entry) = run_shell(
            source_dir,
            &args.output_all,
            args.package_name.as_deref(),
            args.package_version.as_deref(),
        )? {
            manifest.indexes.push(entry);
        }
    }
    if !args.no_po {
        if let Some(entry) = run_po(
            source_dir,
            &args.output_all,
            args.package_name.as_deref(),
            args.package_version.as_deref(),
        )? {
            manifest.indexes.push(entry);
        }
    }
    if !args.no_tree_sitter {
        if let Some(entry) = run_tree_sitter(source_dir, &args.output_all)? {
            manifest.indexes.push(entry);
        }
    }
    Ok(())
}

/// Run `debian-lsp scip` on the source tree to produce `debian.scip`. No-op
/// when the tree has no `debian/` subdirectory (upstream tarballs, non-Debian
/// projects); a non-zero exit or missing binary is a hard error.
fn run_debian_lsp(source_dir: &Path, output_dir: &Path) -> Result<Option<IndexEntry>, RunError> {
    if !source_dir.join("debian").is_dir() {
        log::debug!(
            "No debian/ in {}; skipping debian-lsp",
            source_dir.display()
        );
        return Ok(None);
    }
    let output = output_dir.join("debian.scip");
    log::info!(
        "Generating Debian packaging SCIP index at {}",
        output.display()
    );
    let status = std::process::Command::new("debian-lsp")
        .arg("scip")
        .arg("--output")
        .arg(&output)
        .arg(source_dir)
        .status()
        .map_err(|e| RunError::Setup(format!("failed to run debian-lsp: {}", e)))?;
    if !status.success() {
        return Err(RunError::Setup(format!(
            "debian-lsp exited with {}",
            status
        )));
    }
    Ok(Some(IndexEntry::post_pass(
        "debian.scip",
        "debian-lsp",
        version::probe_host("debian-lsp"),
    )))
}

/// Run `makefile-lsp scip` on `debian/rules` plus any Makefile / *.mk in the
/// source tree, producing a combined `makefile.scip`. No-op when nothing
/// matches; a non-zero exit or missing binary is a hard error otherwise.
fn run_makefile_lsp(source_dir: &Path, output_dir: &Path) -> Result<Option<IndexEntry>, RunError> {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let rules = source_dir.join("debian/rules");
    if rules.is_file() {
        inputs.push(rules);
    }
    collect_makefiles(source_dir, &mut inputs);
    if inputs.is_empty() {
        log::debug!(
            "No Makefile or debian/rules in {}; skipping makefile-lsp",
            source_dir.display()
        );
        return Ok(None);
    }
    let output = output_dir.join("makefile.scip");
    log::info!(
        "Generating Makefile SCIP index at {} ({} file(s))",
        output.display(),
        inputs.len()
    );
    let mut cmd = std::process::Command::new("makefile-lsp");
    cmd.arg("scip")
        .arg("--project-root")
        .arg(source_dir)
        .arg("--output")
        .arg(&output);
    for input in &inputs {
        cmd.arg(input);
    }
    let status = cmd
        .status()
        .map_err(|e| RunError::Setup(format!("failed to run makefile-lsp: {}", e)))?;
    if !status.success() {
        return Err(RunError::Setup(format!(
            "makefile-lsp exited with {}",
            status
        )));
    }
    Ok(Some(IndexEntry::post_pass(
        "makefile.scip",
        "makefile-lsp",
        version::probe_host("makefile-lsp"),
    )))
}

/// Walk `source_dir` and append every Makefile / GNUmakefile / *.mk found to
/// `out`. Skips VCS metadata and common build-output directories so we do not
/// re-index generated Makefiles under target/ or build/.
fn collect_makefiles(source_dir: &Path, out: &mut Vec<PathBuf>) {
    let mut stack = vec![source_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some(".git" | ".hg" | ".svn" | "target" | "build" | "node_modules")
                ) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() && is_makefile(&path) {
                out.push(path);
            }
        }
    }
}

/// Is `path` one of the file names GNU make recognizes as a makefile, or a
/// `*.mk` fragment? Excludes `debian/rules`, which the caller adds explicitly.
fn is_makefile(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if matches!(name, "Makefile" | "makefile" | "GNUmakefile") {
            return true;
        }
    }
    matches!(path.extension().and_then(|s| s.to_str()), Some("mk"))
}

/// Run `scip-shell` on the source tree to produce `shell.scip`. Runs before
/// the tree-sitter pass so its richer tokens win for shell files. A non-zero
/// exit or missing binary is a hard error.
fn run_shell(
    source_dir: &Path,
    output_dir: &Path,
    package_name: Option<&str>,
    package_version: Option<&str>,
) -> Result<Option<IndexEntry>, RunError> {
    let output = output_dir.join("shell.scip");
    log::info!("Generating shell SCIP index at {}", output.display());
    let mut cmd = std::process::Command::new("scip-shell");
    cmd.arg("--project-root").arg(source_dir);
    cmd.arg("--output").arg(&output);
    if let Some(name) = package_name {
        cmd.arg("--package-name").arg(name);
    }
    if let Some(version) = package_version {
        cmd.arg("--package-version").arg(version);
    }
    cmd.arg(source_dir);
    let status = cmd
        .status()
        .map_err(|e| RunError::Setup(format!("failed to run scip-shell: {}", e)))?;
    if !status.success() {
        return Err(RunError::Setup(format!(
            "scip-shell exited with {}",
            status
        )));
    }
    Ok(Some(IndexEntry::post_pass(
        "shell.scip",
        "scip-shell",
        version::probe_host("scip-shell"),
    )))
}

/// Run `scip-po` on the source tree to produce `po.scip`. No-op when the tree
/// has no `.po`/`.pot` files; a non-zero exit or missing binary is a hard
/// error otherwise.
fn run_po(
    source_dir: &Path,
    output_dir: &Path,
    package_name: Option<&str>,
    package_version: Option<&str>,
) -> Result<Option<IndexEntry>, RunError> {
    if !has_po_files(source_dir) {
        log::debug!(
            "No .po/.pot files in {}; skipping scip-po",
            source_dir.display()
        );
        return Ok(None);
    }
    let output = output_dir.join("po.scip");
    log::info!("Generating gettext .po SCIP index at {}", output.display());
    let mut cmd = std::process::Command::new("scip-po");
    cmd.arg("--project-root").arg(source_dir);
    cmd.arg("--output").arg(&output);
    if let Some(name) = package_name {
        cmd.arg("--package-name").arg(name);
    }
    if let Some(version) = package_version {
        cmd.arg("--package-version").arg(version);
    }
    cmd.arg(source_dir);
    let status = cmd
        .status()
        .map_err(|e| RunError::Setup(format!("failed to run scip-po: {}", e)))?;
    if !status.success() {
        return Err(RunError::Setup(format!("scip-po exited with {}", status)));
    }
    Ok(Some(IndexEntry::post_pass(
        "po.scip",
        "scip-po",
        version::probe_host("scip-po"),
    )))
}

/// Does `source_dir` contain any `.po` or `.pot` file? Walks the tree until
/// the first match; used to skip the scip-po pass on projects without
/// translations.
fn has_po_files(source_dir: &Path) -> bool {
    let mut stack = vec![source_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_dir() {
                // Skip VCS metadata; a stray .po in .git/ would be a false
                // positive and we would never index it anyway.
                if entry.file_name() == ".git" {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let ext = path.extension().and_then(|s| s.to_str());
                if matches!(ext, Some("po") | Some("pot")) {
                    return true;
                }
            }
        }
    }
    false
}

/// Run `scip-tree-sitter` to produce `tree-sitter.scip`, covering files no
/// language indexer touched. Passes every other `.scip` in `output_dir` as
/// `--exclude-scip` so the tree-sitter pass defers to their richer tokens. A
/// non-zero exit or missing binary is a hard error.
fn run_tree_sitter(source_dir: &Path, output_dir: &Path) -> Result<Option<IndexEntry>, RunError> {
    let output = output_dir.join("tree-sitter.scip");
    let mut cmd = std::process::Command::new("scip-tree-sitter");
    cmd.arg("--root").arg(source_dir);
    cmd.arg("--output").arg(&output);
    if let Ok(entries) = std::fs::read_dir(output_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path == output {
                continue;
            }
            if path.extension().and_then(|s| s.to_str()) == Some("scip") {
                cmd.arg("--exclude-scip").arg(&path);
            }
        }
    }
    log::info!(
        "Generating tree-sitter syntax SCIP index at {}",
        output.display()
    );
    let status = cmd
        .status()
        .map_err(|e| RunError::Setup(format!("failed to run scip-tree-sitter: {}", e)))?;
    if !status.success() {
        return Err(RunError::Setup(format!(
            "scip-tree-sitter exited with {}",
            status
        )));
    }
    Ok(Some(IndexEntry::post_pass(
        "tree-sitter.scip",
        "scip-tree-sitter",
        version::probe_host("scip-tree-sitter"),
    )))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_makefile() {
        assert!(is_makefile(Path::new("Makefile")));
        assert!(is_makefile(Path::new("makefile")));
        assert!(is_makefile(Path::new("GNUmakefile")));
        assert!(is_makefile(Path::new("sub/Makefile")));
        assert!(is_makefile(Path::new("rules.mk")));
        assert!(is_makefile(Path::new("a/b/config.mk")));
        // debian/rules is not a Makefile by name; caller adds it explicitly.
        assert!(!is_makefile(Path::new("debian/rules")));
        assert!(!is_makefile(Path::new("README.md")));
        assert!(!is_makefile(Path::new("Cargo.toml")));
    }

    #[test]
    fn test_collect_makefiles() {
        let base = std::env::temp_dir().join(format!(
            "ultrascip-collect-makefiles-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        // Layout with one of each accepted name, a *.mk under a subdir, and
        // excluded paths (target/, .git/) that must not be picked up.
        for (rel, is_dir) in [
            ("Makefile", false),
            ("sub", true),
            ("sub/GNUmakefile", false),
            ("sub/rules.mk", false),
            ("target", true),
            ("target/Makefile", false),
            (".git", true),
            (".git/Makefile", false),
            ("node_modules", true),
            ("node_modules/pkg/Makefile", false),
            ("README.md", false),
        ] {
            let p = base.join(rel);
            if is_dir {
                std::fs::create_dir_all(&p).unwrap();
            } else {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent).unwrap();
                }
                std::fs::write(&p, b"").unwrap();
            }
        }

        let mut found: Vec<PathBuf> = Vec::new();
        collect_makefiles(&base, &mut found);
        let mut relative: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        relative.sort();

        std::fs::remove_dir_all(&base).ok();

        assert_eq!(
            relative,
            vec![
                "Makefile".to_string(),
                "sub/GNUmakefile".to_string(),
                "sub/rules.mk".to_string(),
            ]
        );
    }
}
