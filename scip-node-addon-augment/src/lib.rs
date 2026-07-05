//! scip-node-addon-augment reads a C/C++ SCIP index (as produced by
//! scip-clang) and emits a companion SCIP index that adds JavaScript-side
//! symbols for every Node.js native addon export it can identify by
//! scanning the sources.
//!
//! The input SCIP gives us the exact symbol strings scip-clang assigned to
//! each C/C++ function (which we use as the target of the JS -> C
//! Relationship links) and the project metadata to inherit. The source
//! parse is the authoritative source of the JS-facing names, since those
//! live only in string literals inside registration calls.

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

mod addon_scan;

use addon_scan::{JsExport, JsExportKind, ScanResult};

/// Options for [`augment_file`].
#[derive(Debug, Default, Clone)]
pub struct AugmentOptions {
    /// Source tree to scan for `.c`/`.cc`/`.cpp`/`.cxx` files. Defaults to
    /// the input index's `project_root` (or the current directory if none
    /// is set).
    pub source_root: Option<PathBuf>,
    /// JS package name (e.g. "node-libpq"). Falls back to `package.json`
    /// `name` field.
    pub js_package: Option<String>,
    /// JS package version. Falls back to `package.json` `version`, then to
    /// the version of the first C SCIP symbol, then to "0.0.0".
    pub js_version: Option<String>,
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
    /// The source tree contained no Node.js addon exports, or no JS package
    /// name could be inferred; nothing was written.
    NoExports,
}

/// Read the C/C++ SCIP index at `input`, scan `opts.source_root` for Node.js
/// native addon exports, and write a companion JS-side SCIP index to `output`.
///
/// Returns [`AugmentOutcome::NoExports`] and does not write to `output` when
/// the scan finds no exports and no module init, or when the JS package name
/// cannot be determined (no `js_package` given, no `package.json`).
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
        if let Err(err) = addon_scan::scan_file(entry.path(), &source_root, &mut scan) {
            eprintln!("warning: scanning {}: {}", entry.path().display(), err);
        }
    }

    if scan.exports.is_empty() && scan.module_name.is_none() {
        return Ok(AugmentOutcome::NoExports);
    }

    let package_json = read_package_json(&source_root);

    let Some(js_package) = opts
        .js_package
        .clone()
        .or_else(|| package_json.as_ref().and_then(|p| p.name.clone()))
    else {
        // Silently skip: a C project that happens to include Node addon
        // registration calls but ships no package.json isn't necessarily a JS
        // package we can name.
        return Ok(AugmentOutcome::NoExports);
    };

    let js_version = opts
        .js_version
        .clone()
        .or_else(|| package_json.as_ref().and_then(|p| p.version.clone()))
        .or_else(|| first_package_version(&parsed))
        .unwrap_or_else(|| "0.0.0".to_string());

    let c_lookup = CSymbolLookup::from_index(&parsed);
    let augmented = build_output(&parsed, &js_package, &js_version, &scan, &c_lookup);

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
        .map(|s| {
            matches!(
                s,
                "build" | "target" | "node_modules" | "deps" | "third_party"
            ) || s.starts_with('.')
        })
        .unwrap_or(false)
}

fn strip_file_scheme(root: &str) -> Option<PathBuf> {
    root.strip_prefix("file://").map(PathBuf::from)
}

#[derive(Debug, Default)]
struct PackageJson {
    name: Option<String>,
    version: Option<String>,
}

fn read_package_json(source_root: &Path) -> Option<PackageJson> {
    let text = fs::read_to_string(source_root.join("package.json")).ok()?;
    Some(PackageJson {
        name: extract_json_field(&text, "name"),
        version: extract_json_field(&text, "version"),
    })
}

/// Tiny JSON scan for a top-level string field. Doesn't handle nested
/// objects with the same key, but package.json's top level is what we care
/// about and the important fields ("name", "version") appear early in
/// almost every file.
fn extract_json_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let idx = text.find(&needle)?;
    let rest = &text[idx + needle.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    let value = &rest[..end];
    (!value.is_empty()).then(|| value.to_string())
}

fn first_package_version(index: &Index) -> Option<String> {
    for doc in &index.documents {
        for si in &doc.symbols {
            if let Some(v) = symbol_version(&si.symbol) {
                // scip-clang uses `$` as its version placeholder, and `.` is
                // the SCIP empty-field marker.
                if !v.is_empty() && v != "." && v != "$" {
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

/// C/C++ SCIP symbol lookup by identifier.
///
/// scip-clang encodes free functions as `cxx . . $ <name>(<hash>).` and
/// class methods as `cxx . . $ <Class>#<Method>(<hash>).`. We index by
/// three keys per symbol so a lookup by `Connection::Connect` (source
/// form), `Connection#Connect` (SCIP descriptor form), or just `Connect`
/// (bare method name) all succeed.
struct CSymbolLookup {
    by_key: HashMap<String, Vec<String>>,
}

impl CSymbolLookup {
    fn from_index(index: &Index) -> Self {
        let mut by_key: HashMap<String, Vec<String>> = HashMap::new();
        let mut record = |sym: &str| {
            for key in symbol_lookup_keys(sym) {
                by_key.entry(key).or_default().push(sym.to_string());
            }
        };
        for doc in &index.documents {
            for occ in &doc.occurrences {
                if occ.symbol_roles & (SymbolRole::Definition as i32) == 0
                    || occ.symbol.starts_with("local ")
                {
                    continue;
                }
                record(&occ.symbol);
            }
            for si in &doc.symbols {
                if si.symbol.starts_with("local ") {
                    continue;
                }
                record(&si.symbol);
            }
        }
        for v in by_key.values_mut() {
            v.sort();
            v.dedup();
        }
        Self { by_key }
    }

    fn find(&self, ident: &str) -> Option<&str> {
        // Try the ident as-is first, then in SCIP descriptor form
        // (`::` -> `#`), then a fallback keyed on the last path component.
        if let Some(v) = self.by_key.get(ident) {
            return v.first().map(String::as_str);
        }
        let normalised = ident.replace("::", "#");
        if let Some(v) = self.by_key.get(&normalised) {
            return v.first().map(String::as_str);
        }
        let tail = ident.rsplit("::").next().unwrap_or(ident);
        self.by_key
            .get(tail)
            .and_then(|v| v.first().map(String::as_str))
    }
}

/// Yield the set of lookup keys a symbol should be indexed under.
///
/// For a symbol `cxx . . $ Connection#Connect(<hash>).` we index it under
/// `Connection#Connect`, `Connection::Connect`, and `Connect` so any of the
/// three forms our source-scanner might produce hits.
fn symbol_lookup_keys(symbol: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let Some(descriptor) = descriptor_text(symbol) else {
        return keys;
    };
    let core = strip_descriptor_trailer(&descriptor);
    if !core.is_empty() {
        keys.push(core.clone());
        if core.contains('#') {
            keys.push(core.replace('#', "::"));
        }
        if let Some(tail) = core.rsplit(&['#', '/', ':'][..]).next() {
            if tail != core {
                keys.push(tail.to_string());
            }
        }
    }
    keys
}

fn descriptor_text(symbol: &str) -> Option<String> {
    let mut it = symbol.splitn(5, ' ');
    let _ = it.next()?;
    let _ = it.next()?;
    let _ = it.next()?;
    let _ = it.next()?;
    let descriptors = it.next()?;
    let descriptors = descriptors.strip_prefix("$ ").unwrap_or(descriptors);
    Some(descriptors.to_string())
}

/// Strip a trailing `(hash).` or `().` or a trailing suffix char from a
/// descriptor to get the raw ident.
fn strip_descriptor_trailer(desc: &str) -> String {
    let mut out = String::new();
    let mut chars = desc.chars();
    while let Some(c) = chars.next() {
        match c {
            '(' => {
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
                if chars.clone().next() == Some('.') {
                    chars.next();
                }
                break;
            }
            '.' | ':' | '!' if out.is_empty() => continue,
            _ => out.push(c),
        }
    }
    out
}

fn build_output(
    input: &Index,
    js_package: &str,
    js_version: &str,
    scan: &ScanResult,
    c_lookup: &CSymbolLookup,
) -> Index {
    let mut out = Index::new();
    out.metadata = protobuf::MessageField::some(Metadata {
        version: input.metadata.version,
        tool_info: protobuf::MessageField::some(ToolInfo {
            name: "scip-node-addon-augment".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arguments: vec![],
            special_fields: Default::default(),
        }),
        project_root: input.metadata.project_root.clone(),
        text_document_encoding: input.metadata.text_document_encoding,
        special_fields: Default::default(),
    });

    let mut by_path: HashMap<PathBuf, Vec<JsExport>> = HashMap::new();
    for export in &scan.exports {
        by_path
            .entry(export.relative_path.clone())
            .or_default()
            .push(export.clone());
    }

    let module_name = scan.module_name.as_deref().unwrap_or(js_package);

    // Emit a Module export at the first document we visit, if the scanner
    // saw a NAPI_MODULE/NODE_MODULE. Its span is unimportant; we point at
    // (0,0,0).
    let mut wrote_module = false;
    let paths: Vec<PathBuf> = by_path.keys().cloned().collect();
    for relpath in paths {
        let exports = by_path.remove(&relpath).unwrap_or_default();
        let mut doc = Document::new();
        doc.language = "JavaScript".to_string();
        doc.relative_path = relpath.to_string_lossy().into_owned();

        if !wrote_module {
            if let Some(module_name) = &scan.module_name {
                push_module_symbol(
                    &mut doc,
                    js_package,
                    js_version,
                    module_name,
                    scan,
                    c_lookup,
                );
                wrote_module = true;
            }
        }

        for export in &exports {
            emit_export(
                &mut doc,
                export,
                js_package,
                js_version,
                module_name,
                c_lookup,
            );
        }

        if !doc.symbols.is_empty() {
            out.documents.push(doc);
        }
    }

    // If the scanner found a module but no per-function exports, we still
    // want to emit the module symbol. Attach it to a synthetic document
    // keyed to `<module>.js`.
    if !wrote_module {
        if let Some(module_name) = &scan.module_name {
            let mut doc = Document::new();
            doc.language = "JavaScript".to_string();
            doc.relative_path = format!("{}.js", module_name);
            push_module_symbol(
                &mut doc,
                js_package,
                js_version,
                module_name,
                scan,
                c_lookup,
            );
            if !doc.symbols.is_empty() {
                out.documents.push(doc);
            }
        }
    }

    out
}

fn push_module_symbol(
    doc: &mut Document,
    js_package: &str,
    js_version: &str,
    module_name: &str,
    scan: &ScanResult,
    c_lookup: &CSymbolLookup,
) {
    let module_symbol = js_module_symbol(js_package, js_version, module_name);
    let mut si = SymbolInformation::new();
    si.symbol = module_symbol;
    si.display_name = module_name.to_string();
    si.kind = Kind::Module.into();
    if let Some(init) = &scan.module_init {
        if let Some(c_symbol) = c_lookup.find(init) {
            let mut rel = Relationship::new();
            rel.symbol = c_symbol.to_string();
            rel.is_implementation = true;
            rel.is_reference = true;
            rel.is_definition = true;
            si.relationships.push(rel);
        }
    }
    doc.symbols.push(si);
}

fn emit_export(
    doc: &mut Document,
    export: &JsExport,
    js_package: &str,
    js_version: &str,
    module_name: &str,
    c_lookup: &CSymbolLookup,
) {
    let kind = match export.kind {
        JsExportKind::Function => Kind::Function,
        JsExportKind::Method => Kind::Method,
    };
    let symbol = js_export_symbol(js_package, js_version, module_name, &export.js_name);

    let mut si = SymbolInformation::new();
    si.symbol = symbol.clone();
    si.display_name = export.js_name.clone();
    si.kind = kind.into();
    if !export.c_name.is_empty() {
        if let Some(c_symbol) = c_lookup.find(&export.c_name) {
            let mut rel = Relationship::new();
            rel.symbol = c_symbol.to_string();
            rel.is_implementation = true;
            rel.is_reference = true;
            rel.is_definition = true;
            si.relationships.push(rel);
        }
    }
    doc.symbols.push(si);

    let mut occ = Occurrence::new();
    occ.symbol = symbol;
    occ.symbol_roles = SymbolRole::Definition as i32;
    let (line, col_start, col_end) = export.name_span;
    occ.range = vec![line as i32, col_start as i32, col_end as i32];
    doc.occurrences.push(occ);
}

fn js_module_symbol(package: &str, version: &str, module_name: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-typescript".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "npm".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors.push(Descriptor {
        name: module_name.to_string(),
        suffix: Suffix::Namespace.into(),
        ..Default::default()
    });
    sym.descriptors.push(Descriptor {
        name: "__init__".to_string(),
        suffix: Suffix::Meta.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

fn js_export_symbol(package: &str, version: &str, module_name: &str, name: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-typescript".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "npm".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors.push(Descriptor {
        name: module_name.to_string(),
        suffix: Suffix::Namespace.into(),
        ..Default::default()
    });
    sym.descriptors.push(Descriptor {
        name: name.to_string(),
        suffix: Suffix::Method.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_field() {
        let text = r#"{ "name": "node-libpq", "version": "1.10.0", "dependencies": {} }"#;
        assert_eq!(
            extract_json_field(text, "name"),
            Some("node-libpq".to_string())
        );
        assert_eq!(
            extract_json_field(text, "version"),
            Some("1.10.0".to_string())
        );
        assert_eq!(extract_json_field(text, "missing"), None);
    }

    #[test]
    fn js_module_symbol_shape() {
        let s = js_module_symbol("node-libpq", "1.10.0", "addon");
        assert_eq!(s, "scip-typescript npm node-libpq 1.10.0 addon/__init__:");
    }

    #[test]
    fn js_export_symbol_shape() {
        let s = js_export_symbol("node-libpq", "1.10.0", "addon", "$connect");
        assert!(s.starts_with("scip-typescript npm node-libpq 1.10.0 "));
        assert!(s.contains("addon/"));
        assert!(s.ends_with("$connect()."));
    }

    #[test]
    fn symbol_lookup_keys_include_bare_and_qualified() {
        let keys = symbol_lookup_keys("cxx . . $ Connection#Connect(abc).");
        assert!(keys.contains(&"Connection#Connect".to_string()));
        assert!(keys.contains(&"Connection::Connect".to_string()));
        assert!(keys.contains(&"Connect".to_string()));
    }

    #[test]
    fn symbol_lookup_keys_free_function() {
        let keys = symbol_lookup_keys("cxx . . $ init(abc).");
        assert!(keys.contains(&"init".to_string()));
    }

    #[test]
    fn c_symbol_lookup_finds_qualified() {
        let mut index = Index::new();
        let mut doc = Document::new();
        let mut si = SymbolInformation::new();
        si.symbol = "cxx . . $ Connection#Connect(abcd).".to_string();
        doc.symbols.push(si);
        index.documents.push(doc);
        let lookup = CSymbolLookup::from_index(&index);
        assert_eq!(
            lookup.find("Connection::Connect"),
            Some("cxx . . $ Connection#Connect(abcd).")
        );
    }
}
