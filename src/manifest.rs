//! The `manifest.json` written alongside the SCIP indexes.
//!
//! The output directory holds indexes from several producers (language
//! indexers, FFI companion augments, host-side post-passes) and their
//! provenance is not recoverable from the file names alone. The manifest
//! records, per index, which build system and indexer produced it, and which
//! build systems failed to index, so downstream consumers do not have to
//! parse the ultrascip log.

use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Manifest {
    /// Version of ultrascip that produced this output directory.
    pub ultrascip_version: &'static str,
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
            indexes: Vec::new(),
            failures: Vec::new(),
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
    /// The indexer's release tag, when it was downloaded as a release binary
    /// during this run. Unset when the indexer was already present or was
    /// installed through a package manager.
    #[serde(skip_serializing_if = "Option::is_none")]
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
            indexer_version: None,
            documents: Some(documents),
            exports: Some(exports),
        }
    }

    pub fn post_pass(file: &str, indexer: &str) -> Self {
        IndexEntry {
            file: file.to_string(),
            kind: IndexKind::PostPass,
            language: None,
            build_system: None,
            indexer: indexer.to_string(),
            indexer_version: None,
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

    #[test]
    fn test_manifest_json() {
        let manifest = Manifest {
            ultrascip_version: "0.1.0",
            indexes: vec![
                IndexEntry::language(
                    "rust.scip".to_string(),
                    "rust",
                    "cargo",
                    "rust-analyzer",
                    None,
                ),
                IndexEntry::companion(
                    "rust-c-abi.scip".to_string(),
                    "cargo",
                    "scip-c-abi-augment",
                    3,
                    7,
                ),
                IndexEntry::post_pass("debian.scip", "debian-lsp"),
            ],
            failures: vec![IndexFailure {
                build_system: "gradle".to_string(),
                error: "boom".to_string(),
            }],
        };
        assert_eq!(
            serde_json::to_string_pretty(&manifest).unwrap(),
            r#"{
  "ultrascip_version": "0.1.0",
  "indexes": [
    {
      "file": "rust.scip",
      "kind": "language",
      "language": "rust",
      "build_system": "cargo",
      "indexer": "rust-analyzer"
    },
    {
      "file": "rust-c-abi.scip",
      "kind": "companion",
      "build_system": "cargo",
      "indexer": "scip-c-abi-augment",
      "documents": 3,
      "exports": 7
    },
    {
      "file": "debian.scip",
      "kind": "post-pass",
      "indexer": "debian-lsp"
    }
  ],
  "failures": [
    {
      "build_system": "gradle",
      "error": "boom"
    }
  ]
}"#
        );
    }
}
