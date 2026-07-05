//! scip-c-python-augment reads a C SCIP index (as produced by scip-clang) and
//! emits a companion SCIP index that adds Python-side symbols for every
//! CPython C extension export found by scanning the corresponding C sources
//! with tree-sitter.
//!
//! The C SCIP index gives us the exact symbol strings scip-clang assigned to
//! each C function (which we use as the target of the Python -> C relationship
//! links) and the project metadata to inherit. The source parse is the
//! authoritative source of the Python-facing names, since those live only in
//! string literals inside `PyMethodDef` initializers.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use protobuf::Message;
use scip::types::{
    descriptor::Suffix, symbol_information::Kind, Descriptor, Document, Index, Metadata,
    Occurrence, Package, Relationship, Symbol, SymbolInformation, SymbolRole, ToolInfo,
};
use walkdir::WalkDir;

mod c_scan;

use c_scan::{CExport, CExportKind, ScanResult};

/// Options for [`augment_file`].
#[derive(Debug, Default, Clone)]
pub struct AugmentOptions {
    /// Source tree to scan for `.c` files. Defaults to the input index's
    /// `project_root` (or the current directory if none is set).
    pub source_root: Option<PathBuf>,
    /// Python distribution name (e.g. "dulwich"). Falls back to
    /// `[project].name` in pyproject.toml.
    pub python_package: Option<String>,
    /// Python distribution version. Defaults to the version of the first
    /// package seen in the input SCIP, or "0.0.0".
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
    /// The source tree contained no CPython extension exports, or no Python
    /// package name could be inferred; nothing was written.
    NoExports,
}

/// Read the C SCIP index at `input`, scan `opts.source_root` for CPython C
/// extension exports, and write a companion Python-side SCIP index to `output`.
///
/// Returns [`AugmentOutcome::NoExports`] and does not write to `output` when
/// the scan finds no exports or the Python package name cannot be determined
/// (no `python_package` given, no `pyproject.toml`).
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

    // Scan first: a C project with no CPython entry points has nothing to
    // augment, and pyproject.toml lookup would only surface a spurious warning
    // for such a project.
    let mut scan = ScanResult::default();
    for entry in WalkDir::new(&source_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden_or_build(e.file_name()))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                eprintln!("warning: walking {}: {}", source_root.display(), err);
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        match entry.path().extension().and_then(|s| s.to_str()) {
            Some("c") | Some("cc") | Some("cpp") | Some("cxx") => {}
            _ => continue,
        }
        if let Err(err) = c_scan::scan_file(entry.path(), &source_root, &mut scan) {
            eprintln!("warning: scanning {}: {}", entry.path().display(), err);
        }
    }

    if scan.exports.is_empty() {
        return Ok(AugmentOutcome::NoExports);
    }

    let Some(python_package) = opts
        .python_package
        .clone()
        .or_else(|| detect_python_package(&source_root))
    else {
        // Silently skip: a C project that happens to link Python.h but ships
        // no pyproject.toml isn't necessarily a Python extension distribution
        // we can name.
        return Ok(AugmentOutcome::NoExports);
    };

    let python_version = opts
        .python_version
        .clone()
        .or_else(|| first_package_version(&parsed))
        .unwrap_or_else(|| "0.0.0".to_string());

    let c_lookup = CSymbolLookup::from_index(&parsed);
    let augmented = build_output(&parsed, &python_package, &python_version, &scan, &c_lookup);

    let payload = augmented.write_to_bytes()?;
    fs::write(output, &payload)
        .with_context(|| format!("writing output SCIP {}", output.display()))?;

    Ok(AugmentOutcome::Written(AugmentStats {
        documents: augmented.documents.len(),
        exports: scan.exports.len(),
    }))
}

fn is_hidden_or_build(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .map(|s| s == "build" || s == "target" || s.starts_with('.'))
        .unwrap_or(false)
}

fn strip_file_scheme(root: &str) -> Option<PathBuf> {
    root.strip_prefix("file://").map(PathBuf::from)
}

fn first_package_version(index: &Index) -> Option<String> {
    for doc in &index.documents {
        for si in &doc.symbols {
            if let Some(v) = symbol_version(&si.symbol) {
                if !v.is_empty() && v != "." {
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

/// Lookup for C SCIP symbols by their identifier (e.g. "py_is_tree"), used
/// to link each Python symbol we emit to the C definition it wraps.
///
/// scip-clang encodes C symbols as `cxx . . $ <ident>(<hash>).` for functions
/// and `cxx . . $ <ident>.` for globals, where `<hash>` disambiguates
/// definitions across translation units. We store the fully-qualified string
/// so downstream references match exactly.
struct CSymbolLookup {
    by_name: HashMap<String, Vec<String>>,
}

impl CSymbolLookup {
    fn from_index(index: &Index) -> Self {
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for doc in &index.documents {
            for occ in &doc.occurrences {
                if occ.symbol_roles & (SymbolRole::Definition as i32) == 0
                    || occ.symbol.starts_with("local ")
                {
                    continue;
                }
                if let Some(name) = c_symbol_name(&occ.symbol) {
                    by_name.entry(name).or_default().push(occ.symbol.clone());
                }
            }
            for si in &doc.symbols {
                if si.symbol.starts_with("local ") {
                    continue;
                }
                if let Some(name) = c_symbol_name(&si.symbol) {
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

    fn find(&self, ident: &str) -> Option<&str> {
        self.by_name
            .get(ident)
            .and_then(|v| v.first().map(String::as_str))
    }
}

/// Extract the C identifier from a `cxx . . $ <ident>(...)` or `cxx . . $
/// <ident>.` symbol. Anything else yields None.
fn c_symbol_name(symbol: &str) -> Option<String> {
    let mut it = symbol.splitn(5, ' ');
    let _scheme = it.next()?;
    let _manager = it.next()?;
    let _pkg = it.next()?;
    let _version = it.next()?;
    let descriptors = it.next()?;
    // Descriptors we care about here start with `$ <ident>` (the leading `$`
    // is scip-clang's namespace descriptor for the global namespace); or just
    // `<ident>` for other C indexers. Skip the `$ ` prefix if present.
    let descriptors = descriptors.strip_prefix("$ ").unwrap_or(descriptors);
    let mut name = String::new();
    for c in descriptors.chars() {
        if matches!(c, '(' | '.' | '#' | '/' | ':' | '!' | '[') {
            break;
        }
        name.push(c);
    }
    (!name.is_empty()).then_some(name)
}

fn build_output(
    input: &Index,
    python_package: &str,
    python_version: &str,
    scan: &ScanResult,
    c_lookup: &CSymbolLookup,
) -> Index {
    let mut out = Index::new();
    out.metadata = protobuf::MessageField::some(Metadata {
        version: input.metadata.version,
        tool_info: protobuf::MessageField::some(ToolInfo {
            name: "scip-c-python-augment".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arguments: vec![],
            special_fields: Default::default(),
        }),
        project_root: input.metadata.project_root.clone(),
        text_document_encoding: input.metadata.text_document_encoding,
        special_fields: Default::default(),
    });

    let mut by_path: HashMap<PathBuf, Vec<CExport>> = HashMap::new();
    for export in &scan.exports {
        by_path
            .entry(export.relative_path.clone())
            .or_default()
            .push(export.clone());
    }

    for (relpath, exports) in by_path {
        let module_name = exports
            .iter()
            .find(|e| e.kind == CExportKind::Module)
            .map(|e| e.python_name.clone());
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
                c_lookup,
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
    export: &CExport,
    python_package: &str,
    python_version: &str,
    full_module: &str,
    c_lookup: &CSymbolLookup,
) {
    let (symbol, kind) = match export.kind {
        CExportKind::Module => (
            py_module_symbol(python_package, python_version, full_module),
            Kind::Module,
        ),
        CExportKind::Function => (
            py_function_symbol(
                python_package,
                python_version,
                full_module,
                &export.python_name,
            ),
            Kind::Function,
        ),
    };

    let mut si = SymbolInformation::new();
    si.symbol = symbol.clone();
    si.display_name = export.python_name.clone();
    si.kind = kind.into();
    if let Some(c_symbol) = c_lookup.find(&export.c_name) {
        let mut rel = Relationship::new();
        rel.symbol = c_symbol.to_string();
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
    fn c_symbol_name_handles_scip_clang() {
        assert_eq!(
            c_symbol_name("cxx . . $ py_is_tree(134650f196314f84)."),
            Some("py_is_tree".to_string())
        );
    }

    #[test]
    fn c_symbol_name_handles_globals() {
        assert_eq!(
            c_symbol_name("cxx . . $ py_diff_tree_methods."),
            Some("py_diff_tree_methods".to_string())
        );
    }

    #[test]
    fn c_symbol_name_rejects_local() {
        // (local symbols are filtered before c_symbol_name is called, but the
        // parser itself should still return something sensible if handed one)
        assert_eq!(c_symbol_name("local 3"), None);
    }

    #[test]
    fn parses_pyproject_name() {
        let text = r#"
[project]
name = "dulwich"
"#;
        assert_eq!(extract_pyproject_name(text), Some("dulwich".to_string()));
    }

    #[test]
    fn python_module_symbol_shape() {
        let s = py_module_symbol("dulwich", "0.21.7", "dulwich._diff_tree");
        assert_eq!(
            s,
            "scip-python python dulwich 0.21.7 dulwich/_diff_tree/__init__:"
        );
    }

    #[test]
    fn python_function_symbol_shape() {
        let s = py_function_symbol("dulwich", "0.21.7", "dulwich._diff_tree", "_is_tree");
        assert!(s.ends_with("_is_tree()."));
        assert!(s.contains("dulwich/_diff_tree/"));
    }
}
