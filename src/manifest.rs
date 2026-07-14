//! The `manifest.json` written alongside the SCIP indexes.
//!
//! The output directory holds indexes from several producers (language
//! indexers, FFI companion augments, host-side post-passes) and their
//! provenance is not recoverable from the file names alone. The manifest
//! records, per index, which build system and indexer produced it, at which
//! version, and which build systems failed to index, so downstream consumers
//! do not have to parse the ultrascip log.
//!
//! The versions are what makes regeneration decidable: comparing a stored
//! manifest's `generators` against the current toolchain says which indexes
//! were produced by a tool that has since moved on.

use crate::version::Generators;
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Manifest {
    /// Version of ultrascip that produced this output directory.
    pub ultrascip_version: &'static str,
    /// Version of every generator that contributed an index, keyed by tool
    /// name. A null value means the tool ran but could not report a version
    /// (treat the indexes it produced as of unknown age).
    ///
    /// The same versions appear per-index below; this is the summary a
    /// consumer diffs against its current toolchain to decide what to
    /// regenerate, without walking every entry.
    pub generators: Generators,
    pub indexes: Vec<IndexEntry>,
    /// Build systems whose indexer failed. The indexes that did succeed are
    /// still written (and listed above); a non-empty list here means the run
    /// exited non-zero.
    pub failures: Vec<IndexFailure>,
}

impl Manifest {
    pub fn new() -> Self {
        Manifest {
            ultrascip_version: env!("CARGO_PKG_VERSION"),
            generators: Generators::new(),
            indexes: Vec::new(),
            failures: Vec::new(),
        }
    }

    /// Populate `generators` from the versions recorded on the indexes.
    ///
    /// Call once every index has been added. A tool that produced several
    /// indexes (e.g. scip-clang for both cmake and meson) collapses to one
    /// entry; it is the same binary, so the versions agree.
    pub fn collect_generators(&mut self) {
        for index in &self.indexes {
            self.generators
                .insert(index.indexer.clone(), index.indexer_version.clone());
        }
    }

    /// Write the manifest as pretty-printed JSON.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        std::fs::write(path, json)
    }
}

/// What kind of producer wrote an index.
#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexKind {
    /// A language indexer run for a detected build system.
    Language,
    /// An FFI companion derived from a language index.
    Companion,
    /// A host-side post-pass over the source tree.
    PostPass,
}

/// One index file in the output directory.
#[derive(Debug, Serialize)]
pub struct IndexEntry {
    /// File name within the output directory.
    pub file: String,
    pub kind: IndexKind,
    /// The indexed language, for language indexes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The build system the index was generated for; unset for post-passes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_system: Option<String>,
    /// The tool that produced the index: an indexer binary, an augment crate,
    /// or a post-pass binary.
    pub indexer: String,
    /// The version of that tool: a release tag when it was downloaded as a
    /// release binary this run, otherwise what the binary reports for
    /// `--version` (or ultrascip's own version, for the in-process augments).
    ///
    /// Always serialized, null included: a null says the tool ran but could
    /// not report a version, which a consumer must treat as "unknown, assume
    /// stale". Omitting the key would make that indistinguishable from a tool
    /// that never ran.
    pub indexer_version: Option<String>,
    /// Documents in a companion index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<usize>,
    /// Exported symbols in a companion index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exports: Option<usize>,
}

impl IndexEntry {
    pub fn language(
        file: String,
        language: &str,
        build_system: &str,
        indexer: &str,
        indexer_version: Option<String>,
    ) -> Self {
        IndexEntry {
            file,
            kind: IndexKind::Language,
            language: Some(language.to_string()),
            build_system: Some(build_system.to_string()),
            indexer: indexer.to_string(),
            indexer_version,
            documents: None,
            exports: None,
        }
    }

    /// A companion index, written by an augment crate compiled into ultrascip.
    /// Its version is therefore ultrascip's own (see
    /// [`crate::version::augment_version`]).
    pub fn companion(
        file: String,
        build_system: &str,
        indexer: &str,
        documents: usize,
        exports: usize,
    ) -> Self {
        IndexEntry {
            file,
            kind: IndexKind::Companion,
            language: None,
            build_system: Some(build_system.to_string()),
            indexer: indexer.to_string(),
            indexer_version: crate::version::augment_version(),
            documents: Some(documents),
            exports: Some(exports),
        }
    }

    pub fn post_pass(file: &str, indexer: &str, indexer_version: Option<String>) -> Self {
        IndexEntry {
            file: file.to_string(),
            kind: IndexKind::PostPass,
            language: None,
            build_system: None,
            indexer: indexer.to_string(),
            indexer_version,
            documents: None,
            exports: None,
        }
    }
}

/// A build system whose indexer failed.
#[derive(Debug, Serialize)]
pub struct IndexFailure {
    pub build_system: String,
    pub error: String,
}

/// The outcome of [`crate::scip::run_scip_multi`]: the language and companion
/// indexes written, and the build systems that failed.
#[derive(Debug, Default)]
pub struct ScipReport {
    pub indexes: Vec<IndexEntry>,
    pub failures: Vec<IndexFailure>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest with one index of each kind, versions filled in.
    fn sample_manifest() -> Manifest {
        let mut manifest = Manifest {
            ultrascip_version: "0.1.0",
            generators: Generators::new(),
            indexes: vec![
                IndexEntry::language(
                    "rust.scip".to_string(),
                    "rust",
                    "cargo",
                    "rust-analyzer",
                    Some("1.95.0".to_string()),
                ),
                IndexEntry::companion(
                    "rust-c-abi.scip".to_string(),
                    "cargo",
                    "scip-c-abi-augment",
                    3,
                    7,
                ),
                IndexEntry::post_pass("debian.scip", "debian-lsp", Some("0.1.10".to_string())),
                // A generator that could not report a version.
                IndexEntry::post_pass("shell.scip", "scip-shell", None),
            ],
            failures: vec![IndexFailure {
                build_system: "gradle".to_string(),
                error: "boom".to_string(),
            }],
        };
        manifest.collect_generators();
        manifest
    }

    #[test]
    fn test_manifest_json() {
        let manifest = sample_manifest();
        let augment = crate::version::augment_version().unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&manifest).unwrap(),
            format!(
                r#"{{
  "ultrascip_version": "0.1.0",
  "generators": {{
    "debian-lsp": "0.1.10",
    "rust-analyzer": "1.95.0",
    "scip-c-abi-augment": "{augment}",
    "scip-shell": null
  }},
  "indexes": [
    {{
      "file": "rust.scip",
      "kind": "language",
      "language": "rust",
      "build_system": "cargo",
      "indexer": "rust-analyzer",
      "indexer_version": "1.95.0"
    }},
    {{
      "file": "rust-c-abi.scip",
      "kind": "companion",
      "build_system": "cargo",
      "indexer": "scip-c-abi-augment",
      "indexer_version": "{augment}",
      "documents": 3,
      "exports": 7
    }},
    {{
      "file": "debian.scip",
      "kind": "post-pass",
      "indexer": "debian-lsp",
      "indexer_version": "0.1.10"
    }},
    {{
      "file": "shell.scip",
      "kind": "post-pass",
      "indexer": "scip-shell",
      "indexer_version": null
    }}
  ],
  "failures": [
    {{
      "build_system": "gradle",
      "error": "boom"
    }}
  ]
}}"#
            )
        );
    }

    #[test]
    fn test_collect_generators() {
        let manifest = sample_manifest();
        // Every generator that produced an index is listed, including the one
        // whose version is unknown: a missing key means "did not run", a null
        // means "ran, version unknown". Those must not be conflated.
        assert_eq!(
            manifest.generators,
            Generators::from([
                ("rust-analyzer".to_string(), Some("1.95.0".to_string())),
                ("debian-lsp".to_string(), Some("0.1.10".to_string())),
                (
                    "scip-c-abi-augment".to_string(),
                    crate::version::augment_version()
                ),
                ("scip-shell".to_string(), None),
            ])
        );
    }

    #[test]
    fn test_collect_generators_dedupes_one_tool_many_indexes() {
        // scip-clang indexes both cmake and meson in the same project. It is
        // one binary, so it collapses to a single generators entry.
        let mut manifest = Manifest::new();
        manifest.indexes = vec![
            IndexEntry::language(
                "cpp.scip".to_string(),
                "cpp",
                "cmake",
                "scip-clang",
                Some("0.4.0".to_string()),
            ),
            IndexEntry::language(
                "cpp-meson.scip".to_string(),
                "cpp",
                "meson",
                "scip-clang",
                Some("0.4.0".to_string()),
            ),
        ];
        manifest.collect_generators();
        assert_eq!(
            manifest.generators,
            Generators::from([("scip-clang".to_string(), Some("0.4.0".to_string()))])
        );
    }
}
