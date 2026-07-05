//! scip-pyo3-augment reads a Rust SCIP index (as produced by rust-analyzer)
//! and emits a companion SCIP index that adds Python-side symbols for every
//! PyO3 export found by scanning the corresponding Rust source with `syn`.
//!
//! Detection is source-driven: `#[pyfunction]`, `#[pymodule]`, `#[pyclass]`
//! and `#[pymethods]` items each become a Python `SymbolInformation` under
//! `scip-python python <pkg> <ver> <module>/<name>.`. The input SCIP index is
//! consulted for the Rust symbol string of each item so the emitted Python
//! symbols carry a `Relationship` back to the underlying Rust definition,
//! and for the Rust package version to reuse when the Python distribution
//! version isn't given.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use protobuf::Message;
use scip::types::{
    descriptor::Suffix, symbol_information::Kind, Descriptor, Document, Index, Metadata,
    Occurrence, Package, Relationship, Symbol, SymbolInformation, SymbolRole, ToolInfo,
};
use walkdir::WalkDir;

mod pyo3_scan;

use pyo3_scan::{PyExport, PyExportKind, ScanResult};

/// Options for [`augment_file`].
#[derive(Debug, Default, Clone)]
pub struct AugmentOptions {
    /// Source tree to scan for `#[pyfunction]` etc. Defaults to the input
    /// index's `project_root` (or the current directory if none is set).
    pub source_root: Option<PathBuf>,
    /// Python distribution name (e.g. "dulwich"). Falls back to
    /// `[project].name` in pyproject.toml or the first
    /// `RustExtension("<name>.…")` in setup.py.
    pub python_package: Option<String>,
    /// Python distribution version. Defaults to the version of the first
    /// Rust package seen in the input SCIP, or "0.0.0".
    pub python_version: Option<String>,
}

/// Summary of the augmentation run.
#[derive(Debug, Clone, Copy, Default)]
pub struct AugmentStats {
    pub documents: usize,
    pub exports: usize,
}

/// Outcome of an augmentation call.
#[derive(Debug, Clone)]
pub enum AugmentOutcome {
    /// A companion index was produced and written to the output path.
    Written(AugmentStats),
    /// The source tree contained no PyO3 exports; nothing was written.
    NoExports,
}

/// Read the Rust SCIP index at `input`, scan `opts.source_root` for PyO3
/// exports, and write a companion Python-side SCIP index to `output`.
///
/// If the scan finds no exports, returns [`AugmentOutcome::NoExports`] and
/// does not write to `output`. Callers can use this to decide whether to
/// keep or delete a placeholder file.
pub fn augment_file(input: &Path, output: &Path, opts: &AugmentOptions) -> Result<AugmentOutcome> {
    let bytes =
        fs::read(input).with_context(|| format!("reading input SCIP {}", input.display()))?;
    let parsed: Index = Message::parse_from_bytes(&bytes)
        .with_context(|| format!("parsing input SCIP {}", input.display()))?;

    let source_root = opts
        .source_root
        .clone()
        .or_else(|| {
            let root = parsed.metadata.project_root.clone();
            (!root.is_empty())
                .then(|| strip_file_scheme(&root).unwrap_or_else(|| PathBuf::from(root)))
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Scan first: if there are no PyO3 exports, the project isn't a PyO3
    // project, and we bail before requiring metadata (pyproject.toml, setup.py)
    // that a plain Rust crate legitimately doesn't have.
    let mut scan = ScanResult::default();
    for entry in WalkDir::new(&source_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden_or_target(e.file_name()))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!("warning: walking {}: {}", source_root.display(), err);
                continue;
            }
        };
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|s| s.to_str()) != Some("rs")
        {
            continue;
        }
        if let Err(err) = pyo3_scan::scan_file(entry.path(), &source_root, &mut scan) {
            eprintln!("warning: scanning {}: {}", entry.path().display(), err);
        }
    }

    if scan.exports.is_empty() {
        return Ok(AugmentOutcome::NoExports);
    }

    let python_package = opts
        .python_package
        .clone()
        .or_else(|| detect_python_package(&source_root))
        .ok_or_else(|| {
            anyhow!(
                "could not determine Python package name; pass python_package (looked in {})",
                source_root.display()
            )
        })?;

    let python_version = opts
        .python_version
        .clone()
        .or_else(|| first_rust_package_version(&parsed))
        .unwrap_or_else(|| "0.0.0".to_string());

    let rust_lookup = RustSymbolLookup::from_index(&parsed);
    let augmented = build_output(
        &parsed,
        &python_package,
        &python_version,
        &scan,
        &rust_lookup,
    );

    let payload = augmented.write_to_bytes()?;
    fs::write(output, &payload)
        .with_context(|| format!("writing output SCIP {}", output.display()))?;

    Ok(AugmentOutcome::Written(AugmentStats {
        documents: augmented.documents.len(),
        exports: scan.exports.len(),
    }))
}

fn is_hidden_or_target(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .map(|s| s == "target" || s.starts_with('.'))
        .unwrap_or(false)
}

fn strip_file_scheme(root: &str) -> Option<PathBuf> {
    root.strip_prefix("file://").map(PathBuf::from)
}

fn first_rust_package_version(index: &Index) -> Option<String> {
    for doc in &index.documents {
        for si in &doc.symbols {
            if let Some(v) = symbol_version(&si.symbol) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn symbol_version(symbol: &str) -> Option<String> {
    if symbol.starts_with("local ") {
        return None;
    }
    let mut it = symbol.splitn(5, ' ');
    let _scheme = it.next()?;
    let _manager = it.next()?;
    let _name = it.next()?;
    let version = it.next()?;
    Some(version.to_string())
}

fn detect_python_package(source_root: &Path) -> Option<String> {
    if let Ok(text) = fs::read_to_string(source_root.join("pyproject.toml")) {
        if let Some(name) = extract_pyproject_name(&text) {
            return Some(name);
        }
    }
    if let Ok(text) = fs::read_to_string(source_root.join("setup.py")) {
        if let Some(name) = extract_setup_py_package(&text) {
            return Some(name);
        }
    }
    None
}

fn extract_pyproject_name(text: &str) -> Option<String> {
    let mut in_project = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_project = line == "[project]";
            continue;
        }
        if !in_project {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start().strip_prefix('=')?.trim();
            let rest = rest.trim_matches(|c| c == '"' || c == '\'');
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn extract_setup_py_package(text: &str) -> Option<String> {
    for (i, _) in text.match_indices("RustExtension(") {
        let after = text[i + "RustExtension(".len()..].trim_start();
        let quote = after.chars().next()?;
        if quote != '"' && quote != '\'' {
            continue;
        }
        let rest = &after[1..];
        let end = rest.find(quote)?;
        let dotted = &rest[..end];
        let head = dotted.split('.').next()?;
        if !head.is_empty() {
            return Some(head.to_string());
        }
    }
    None
}

/// Rust SCIP symbol lookup by terminal-descriptor name, used to link each
/// Python symbol we emit back to the Rust definition that implements it.
struct RustSymbolLookup {
    by_name: HashMap<String, Vec<String>>,
}

impl RustSymbolLookup {
    fn from_index(index: &Index) -> Self {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for doc in &index.documents {
            for occ in &doc.occurrences {
                if occ.symbol_roles & (SymbolRole::Definition as i32) == 0
                    || occ.symbol.starts_with("local ")
                {
                    continue;
                }
                if let Some(name) = terminal_descriptor_name(&occ.symbol) {
                    by_name.entry(name).or_default().push(occ.symbol.clone());
                }
            }
            for si in &doc.symbols {
                if si.symbol.starts_with("local ") {
                    continue;
                }
                if let Some(name) = terminal_descriptor_name(&si.symbol) {
                    by_name.entry(name).or_default().push(si.symbol.clone());
                }
            }
        }
        for v in by_name.values_mut() {
            v.sort();
            v.dedup();
        }
        Self { by_name }
    }

    fn find(&self, name: &str) -> Option<&str> {
        self.by_name
            .get(name)
            .and_then(|v| v.first().map(String::as_str))
    }
}

/// Extract the last identifier from the descriptor portion of a SCIP symbol
/// string.
fn terminal_descriptor_name(symbol: &str) -> Option<String> {
    let mut it = symbol.splitn(5, ' ');
    let _ = it.next()?;
    let _ = it.next()?;
    let _ = it.next()?;
    let _ = it.next()?;
    let descriptors = it.next()?;
    let mut last_name: Option<String> = None;
    let mut current = String::new();
    let mut chars = descriptors.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' | '#' | '.' | '!' | ':' => {
                if !current.is_empty() {
                    last_name = Some(current.clone());
                    current.clear();
                }
            }
            '(' => {
                if !current.is_empty() {
                    last_name = Some(current.clone());
                    current.clear();
                    let mut depth = 1;
                    for c2 in chars.by_ref() {
                        match c2 {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    if chars.peek() == Some(&'.') {
                        chars.next();
                    }
                } else {
                    let mut inner = String::new();
                    for c2 in chars.by_ref() {
                        if c2 == ')' {
                            break;
                        }
                        inner.push(c2);
                    }
                    if !inner.is_empty() {
                        last_name = Some(inner);
                    }
                }
            }
            '[' => {
                let mut inner = String::new();
                for c2 in chars.by_ref() {
                    if c2 == ']' {
                        break;
                    }
                    inner.push(c2);
                }
                if !inner.is_empty() {
                    last_name = Some(inner);
                }
            }
            _ => current.push(c),
        }
    }
    last_name
}

fn build_output(
    input: &Index,
    python_package: &str,
    python_version: &str,
    scan: &ScanResult,
    rust_lookup: &RustSymbolLookup,
) -> Index {
    let mut out = Index::new();
    out.metadata = protobuf::MessageField::some(Metadata {
        version: input.metadata.version,
        tool_info: protobuf::MessageField::some(ToolInfo {
            name: "scip-pyo3-augment".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arguments: vec![],
            special_fields: Default::default(),
        }),
        project_root: input.metadata.project_root.clone(),
        text_document_encoding: input.metadata.text_document_encoding,
        special_fields: Default::default(),
    });

    let mut by_path: HashMap<PathBuf, Vec<PyExport>> = HashMap::new();
    for export in &scan.exports {
        by_path
            .entry(export.relative_path.clone())
            .or_default()
            .push(export.clone());
    }

    for (relpath, exports) in by_path {
        let module_name = exports
            .iter()
            .find(|e| matches!(e.kind, PyExportKind::Module))
            .map(|e| e.python_name.clone())
            .or_else(|| exports.first().map(|e| e.python_name.clone()));
        let Some(module_name) = module_name.filter(|s| !s.is_empty()) else {
            continue;
        };
        let full_module = format!("{}.{}", python_package, module_name);

        let mut doc = Document::new();
        doc.language = "Python".to_string();
        doc.relative_path = relpath.to_string_lossy().into_owned();

        for export in &exports {
            emit_export(
                &mut doc,
                export,
                python_package,
                python_version,
                &full_module,
                rust_lookup,
            );
        }

        if !doc.symbols.is_empty() {
            out.documents.push(doc);
        }
    }

    out
}

fn emit_export(
    doc: &mut Document,
    export: &PyExport,
    python_package: &str,
    python_version: &str,
    full_module: &str,
    rust_lookup: &RustSymbolLookup,
) {
    let (symbol, kind, display_name) = match &export.kind {
        PyExportKind::Module => (
            py_module_symbol(python_package, python_version, full_module),
            Kind::Module,
            export.python_name.clone(),
        ),
        PyExportKind::Function => (
            py_function_symbol(
                python_package,
                python_version,
                full_module,
                &export.python_name,
            ),
            Kind::Function,
            export.python_name.clone(),
        ),
        PyExportKind::Class => (
            py_class_symbol(
                python_package,
                python_version,
                full_module,
                &export.python_name,
            ),
            Kind::Class,
            export.python_name.clone(),
        ),
        PyExportKind::Method { class } => (
            py_method_symbol(
                python_package,
                python_version,
                full_module,
                class,
                &export.python_name,
            ),
            Kind::Method,
            export.python_name.clone(),
        ),
    };

    let mut si = SymbolInformation::new();
    si.symbol = symbol.clone();
    si.display_name = display_name;
    si.kind = kind.into();
    if let Some(rust_symbol) = rust_lookup.find(&export.rust_name) {
        let mut rel = Relationship::new();
        rel.symbol = rust_symbol.to_string();
        rel.is_implementation = true;
        rel.is_reference = true;
        rel.is_definition = true;
        si.relationships.push(rel);
    }
    doc.symbols.push(si);

    let mut occ = Occurrence::new();
    occ.symbol = symbol;
    occ.symbol_roles = SymbolRole::Definition as i32;
    let (line, col_start, col_end) = export.name_span;
    occ.range = vec![line as i32, col_start as i32, col_end as i32];
    doc.occurrences.push(occ);
}

fn py_module_symbol(package: &str, version: &str, dotted_module: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-python".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "python".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors = module_descriptors(dotted_module);
    sym.descriptors.push(Descriptor {
        name: "__init__".to_string(),
        suffix: Suffix::Meta.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

fn py_function_symbol(package: &str, version: &str, dotted_module: &str, fn_name: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-python".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "python".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors = module_descriptors(dotted_module);
    sym.descriptors.push(Descriptor {
        name: fn_name.to_string(),
        suffix: Suffix::Method.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

fn py_class_symbol(package: &str, version: &str, dotted_module: &str, class_name: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-python".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "python".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors = module_descriptors(dotted_module);
    sym.descriptors.push(Descriptor {
        name: class_name.to_string(),
        suffix: Suffix::Type.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

fn py_method_symbol(
    package: &str,
    version: &str,
    dotted_module: &str,
    class_name: &str,
    method_name: &str,
) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-python".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "python".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors = module_descriptors(dotted_module);
    sym.descriptors.push(Descriptor {
        name: class_name.to_string(),
        suffix: Suffix::Type.into(),
        ..Default::default()
    });
    sym.descriptors.push(Descriptor {
        name: method_name.to_string(),
        suffix: Suffix::Method.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

fn module_descriptors(dotted_module: &str) -> Vec<Descriptor> {
    dotted_module
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|s| Descriptor {
            name: s.to_string(),
            suffix: Suffix::Namespace.into(),
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pyproject_name() {
        let text = r#"
[build-system]
requires = ["setuptools"]

[project]
name = "dulwich"
description = "Python Git Library"
"#;
        assert_eq!(extract_pyproject_name(text), Some("dulwich".to_string()));
    }

    #[test]
    fn parses_setup_py_rust_extension() {
        let text = r#"
        rust_extensions = [
            RustExtension(
                "dulwich._objects",
                "crates/objects/Cargo.toml",
"#;
        assert_eq!(extract_setup_py_package(text), Some("dulwich".to_string()));
    }

    #[test]
    fn terminal_descriptor_returns_last_name() {
        let sym = "rust-analyzer cargo x 1.0.0 crate/mod/_count_blocks().";
        assert_eq!(
            terminal_descriptor_name(sym),
            Some("_count_blocks".to_string())
        );
    }

    #[test]
    fn python_function_symbol_shape() {
        let s = py_function_symbol("dulwich", "1.2.5", "dulwich._diff_tree", "_count_blocks");
        assert!(s.starts_with("scip-python python dulwich 1.2.5 "));
        assert!(s.ends_with("_count_blocks()."), "got: {}", s);
        assert!(s.contains("dulwich/_diff_tree/"), "got: {}", s);
    }

    #[test]
    fn python_module_symbol_shape() {
        let s = py_module_symbol("dulwich", "1.2.5", "dulwich._pack");
        assert_eq!(
            s,
            "scip-python python dulwich 1.2.5 dulwich/_pack/__init__:"
        );
    }
}
