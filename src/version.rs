//! Version probing for the tools that produce SCIP indexes.
//!
//! Every index in the output directory is only as current as the tool that
//! wrote it, so the manifest records a version per generator: a consumer
//! comparing a stored manifest against the current toolchain can tell which
//! indexes are stale without re-running anything.
//!
//! Generators come in three kinds and none of them report a version the same
//! way. Release binaries (scip-clang, scip-go, scip-java) have a release tag
//! known only when this run downloaded them. Indexers installed into the
//! session (rust-analyzer, scip-python, ...) and host-side post-passes
//! (debian-lsp, scip-shell, ...) have to be asked. In-process augment crates
//! are compiled into ultrascip and know their version statically.
//!
//! A tool that cannot report a version is not an error: the index it wrote is
//! still valid and still listed. Its version records as null, which a consumer
//! reads as "unknown, assume stale" rather than as "not run".

use ognibuild::session::Session;
use std::collections::BTreeMap;

/// Versions of every generator that contributed to an output directory, keyed
/// by tool name. `None` means the tool ran but could not report a version.
///
/// A BTreeMap so the manifest's key order is stable across runs: two manifests
/// for the same toolchain compare equal byte for byte.
pub type Generators = BTreeMap<String, Option<String>>;

/// Normalize the `--version` output of a tool into a bare version string.
///
/// Tools disagree on the shape: rust-analyzer prints `rust-analyzer 1.95.0`,
/// scip-python prints a bare `0.6.6`, others prefix a `v`. Strip a leading
/// tool-name prefix and a `v`, and take the first line, so the recorded value
/// is comparable across tools.
///
/// Returns None when the output carries nothing version-like, so a tool that
/// prints a usage message (or nothing) on `--version` records as unknown
/// rather than as a bogus version string.
pub fn normalize(binary: &str, output: &str) -> Option<String> {
    let line = output.lines().find(|l| !l.trim().is_empty())?.trim();
    // Drop a leading tool name, however the tool spells it: `rust-analyzer
    // 1.95.0` and `scip-go v0.2.7` both reduce to the version alone.
    let rest = line.strip_prefix(binary).map_or(line, str::trim_start);
    let token = rest.split_whitespace().next()?;
    let token = token.strip_prefix('v').unwrap_or(token);
    // A version has to start with a digit. This rejects a usage message or an
    // error that a tool without a --version flag prints instead.
    if !token.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(token.to_string())
}

/// Strip the leading `v` from a GitHub release tag (`v0.12.3` -> `0.12.3`), so
/// a version resolved from a release tag is comparable with one a binary
/// reports for `--version`.
pub fn strip_tag_prefix(tag: &str) -> String {
    tag.strip_prefix('v').unwrap_or(tag).to_string()
}

/// Ask a binary inside the session for its version via `--version`.
///
/// Returns None (and warns) when the binary is absent, exits non-zero, or
/// prints nothing version-like: an index written by a tool that cannot report
/// its version is still a valid index.
pub fn probe_session(session: &dyn Session, binary: &str) -> Option<String> {
    let output = session
        .command(vec![binary, "--version"])
        .quiet(true)
        .check_output()
        .ok()?;
    let text = String::from_utf8(output).ok()?;
    let version = normalize(binary, &text);
    if version.is_none() {
        log::warn!(
            "Could not determine version of {}; recording it as unknown",
            binary
        );
    }
    version
}

/// Ask a binary on the host for its version via `--version`. The post-passes
/// run on the host rather than in the session, so they are probed there.
///
/// Same contract as [`probe_session`]: None, with a warning, when the version
/// cannot be determined.
pub fn probe_host(binary: &str) -> Option<String> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        // A tool without a --version flag may fall through to reading stdin
        // (this is what makefile-lsp did before it grew the flag). Give it no
        // stdin so it cannot block the run waiting for input.
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        log::warn!(
            "{} --version exited with {}; recording its version as unknown",
            binary,
            output.status
        );
        return None;
    }
    // Some tools print the version to stderr rather than stdout; accept either.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = normalize(binary, &stdout).or_else(|| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        normalize(binary, &stderr)
    });
    if version.is_none() {
        log::warn!(
            "Could not determine version of {}; recording it as unknown",
            binary
        );
    }
    version
}

/// The version of an in-process augment crate.
///
/// The FFI companion augments are linked into ultrascip rather than run as
/// separate binaries, so their version is ultrascip's own: rebuilding
/// ultrascip is what changes their output.
pub fn augment_version() -> Option<String> {
    Some(env!("CARGO_PKG_VERSION").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_prefixed() {
        // rust-analyzer prints its own name first.
        assert_eq!(
            normalize("rust-analyzer", "rust-analyzer 1.95.0\n"),
            Some("1.95.0".to_string())
        );
    }

    #[test]
    fn test_normalize_bare() {
        // scip-python prints a bare version.
        assert_eq!(
            normalize("scip-python", "0.6.6\n"),
            Some("0.6.6".to_string())
        );
    }

    #[test]
    fn test_normalize_v_prefix() {
        assert_eq!(
            normalize("scip-go", "scip-go v0.2.7\n"),
            Some("0.2.7".to_string())
        );
        assert_eq!(normalize("scip-go", "v0.2.7\n"), Some("0.2.7".to_string()));
    }

    #[test]
    fn test_normalize_trailing_detail() {
        // Extra build metadata after the version is dropped.
        assert_eq!(
            normalize("scip-java", "scip-java 0.12.3 (build abc123)\n"),
            Some("0.12.3".to_string())
        );
    }

    #[test]
    fn test_normalize_skips_blank_lines() {
        assert_eq!(
            normalize("debian-lsp", "\n\ndebian-lsp 0.1.10\n"),
            Some("0.1.10".to_string())
        );
    }

    #[test]
    fn test_strip_tag_prefix() {
        // A release tag and a --version string must normalize to the same
        // thing, or a consumer diffing manifests sees a spurious change when
        // an indexer switches from downloaded to preinstalled.
        assert_eq!(strip_tag_prefix("v0.12.3"), "0.12.3");
        assert_eq!(strip_tag_prefix("0.12.3"), "0.12.3");
    }

    #[test]
    fn test_normalize_rejects_non_version() {
        // A tool with no --version flag prints usage (or nothing) instead;
        // recording "Usage:" as a version would be worse than recording
        // nothing, because a consumer would compare it and see no change.
        assert_eq!(normalize("makefile-lsp", "Usage: makefile-lsp\n"), None);
        assert_eq!(normalize("makefile-lsp", ""), None);
        assert_eq!(normalize("makefile-lsp", "\n  \n"), None);
    }
}
