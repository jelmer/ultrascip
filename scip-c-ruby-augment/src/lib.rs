//! scip-c-ruby-augment reads a C/C++ SCIP index (as produced by scip-clang)
//! and emits a companion SCIP index that adds Ruby-side symbols for every
//! Ruby C extension registration found by scanning the sources with
//! tree-sitter.
//!
//! The input SCIP gives us the exact symbol strings scip-clang assigned to
//! each C function (which we use as the target of the Ruby -> C relationship
//! links) and the project metadata to inherit. The source parse is the
//! authoritative source of the Ruby-facing names, since those live only in
//! string literals inside `rb_define_*` calls.

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

mod ruby_scan;

use ruby_scan::{RubyExport, RubyExportKind, ScanResult};

/// Options for [`augment_file`].
#[derive(Debug, Default, Clone)]
pub struct AugmentOptions {
    /// Source tree to scan for C/C++ files. Defaults to the input index's
    /// `project_root` (or the current directory if none is set).
    pub source_root: Option<PathBuf>,
    /// Gem name (e.g. "nokogiri"). Falls back to the `name` attribute of a
    /// `*.gemspec` in the source root.
    pub ruby_gem: Option<String>,
    /// Gem version. Falls back to the gemspec's `version` attribute if it is
    /// a string literal, then to the version of the first package seen in
    /// the input SCIP, then to "0.0.0".
    pub ruby_version: Option<String>,
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
    /// The source tree contained no Ruby C extension registrations, or no
    /// gem name could be inferred; nothing was written.
    NoExports,
}

/// Read the C/C++ SCIP index at `input`, scan `opts.source_root` for Ruby C
/// extension registrations, and write a companion Ruby-side SCIP index to
/// `output`.
///
/// Returns [`AugmentOutcome::NoExports`] and does not write to `output` when
/// the scan finds no registrations or the gem name cannot be determined (no
/// `ruby_gem` given, no `*.gemspec`).
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

    // Scan first: a C project with no Ruby entry points has nothing to
    // augment, and gemspec lookup would only surface a spurious warning for
    // such a project.
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
        if let Err(err) = ruby_scan::scan_file(entry.path(), &source_root, &mut scan) {
            eprintln!("warning: scanning {}: {}", entry.path().display(), err);
        }
    }

    if scan.exports.is_empty() {
        return Ok(AugmentOutcome::NoExports);
    }

    let gemspec = read_gemspec(&source_root);

    let Some(ruby_gem) = opts
        .ruby_gem
        .clone()
        .or_else(|| gemspec.as_ref().and_then(|g| g.name.clone()))
    else {
        // Silently skip: a C project that happens to call rb_define_* but
        // ships no gemspec isn't necessarily a gem we can name.
        return Ok(AugmentOutcome::NoExports);
    };

    let ruby_version = opts
        .ruby_version
        .clone()
        .or_else(|| gemspec.as_ref().and_then(|g| g.version.clone()))
        .or_else(|| first_package_version(&parsed))
        .unwrap_or_else(|| "0.0.0".to_string());

    let c_lookup = CSymbolLookup::from_index(&parsed);
    let augmented = build_output(&parsed, &ruby_gem, &ruby_version, &scan, &c_lookup);

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
        .map(|s| matches!(s, "build" | "target" | "tmp" | "vendor") || s.starts_with('.'))
        .unwrap_or(false)
}

fn strip_file_scheme(root: &str) -> Option<PathBuf> {
    root.strip_prefix("file://").map(PathBuf::from)
}

#[derive(Debug, Default)]
struct Gemspec {
    name: Option<String>,
    version: Option<String>,
}

/// Read the first `*.gemspec` in the source root and extract the `name` and
/// `version` attributes, when they are simple string literals. A version
/// referencing a constant (`Foo::VERSION`) yields None.
fn read_gemspec(source_root: &Path) -> Option<Gemspec> {
    let entries = fs::read_dir(source_root).ok()?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("gemspec"))
        .collect();
    paths.sort();
    let text = fs::read_to_string(paths.first()?).ok()?;
    Some(Gemspec {
        name: extract_gemspec_attr(&text, "name"),
        version: extract_gemspec_attr(&text, "version"),
    })
}

/// Extract a `<recv>.<attr> = "value"` assignment from gemspec text. Only
/// string literals are recognised.
fn extract_gemspec_attr(text: &str, attr: &str) -> Option<String> {
    let needle = format!(".{}", attr);
    for line in text.lines() {
        let line = line.trim();
        let Some(idx) = line.find(&needle) else {
            continue;
        };
        let rest = line[idx + needle.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(quote) = rest.chars().next().filter(|c| *c == '"' || *c == '\'') else {
            continue;
        };
        let rest = &rest[1..];
        let Some(end) = rest.find(quote) else {
            continue;
        };
        let value = &rest[..end];
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
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

/// Lookup for C SCIP symbols by their identifier (e.g. "foo_bar"), used to
/// link each Ruby symbol we emit to the C definition it wraps.
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
    ruby_gem: &str,
    ruby_version: &str,
    scan: &ScanResult,
    c_lookup: &CSymbolLookup,
) -> Index {
    let mut out = Index::new();
    out.metadata = protobuf::MessageField::some(Metadata {
        version: input.metadata.version,
        tool_info: protobuf::MessageField::some(ToolInfo {
            name: "scip-c-ruby-augment".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arguments: vec![],
            special_fields: Default::default(),
        }),
        project_root: input.metadata.project_root.clone(),
        text_document_encoding: input.metadata.text_document_encoding,
        special_fields: Default::default(),
    });

    let mut by_path: HashMap<PathBuf, Vec<RubyExport>> = HashMap::new();
    for export in &scan.exports {
        by_path
            .entry(export.relative_path.clone())
            .or_default()
            .push(export.clone());
    }

    for (relpath, exports) in by_path {
        let mut doc = Document::new();
        doc.language = "Ruby".to_string();
        doc.relative_path = relpath.to_string_lossy().into_owned();

        for export in &exports {
            emit_export(&mut doc, export, ruby_gem, ruby_version, c_lookup);
        }

        if !doc.symbols.is_empty() {
            out.documents.push(doc);
        }
    }

    out
}

fn emit_export(
    doc: &mut Document,
    export: &RubyExport,
    ruby_gem: &str,
    ruby_version: &str,
    c_lookup: &CSymbolLookup,
) {
    let kind = match export.kind {
        RubyExportKind::Module => Kind::Module,
        RubyExportKind::Class => Kind::Class,
        RubyExportKind::Method => Kind::Method,
        RubyExportKind::SingletonMethod => Kind::StaticMethod,
    };
    let is_method = matches!(
        export.kind,
        RubyExportKind::Method | RubyExportKind::SingletonMethod
    );
    let symbol = ruby_symbol(
        ruby_gem,
        ruby_version,
        &export.namespace,
        &export.ruby_name,
        is_method,
    );

    let mut si = SymbolInformation::new();
    si.symbol = symbol.clone();
    si.display_name = export.ruby_name.clone();
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

/// Build a scip-ruby-style symbol: namespace components are Type descriptors
/// (`Foo#Bar#`), methods get a Method descriptor (`baz().`).
fn ruby_symbol(gem: &str, version: &str, namespace: &str, name: &str, is_method: bool) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "scip-ruby".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "gem".to_string(),
        name: gem.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    for part in namespace.split("::").filter(|s| !s.is_empty()) {
        sym.descriptors.push(Descriptor {
            name: part.to_string(),
            suffix: Suffix::Type.into(),
            ..Default::default()
        });
    }
    sym.descriptors.push(Descriptor {
        name: name.to_string(),
        suffix: if is_method {
            Suffix::Method.into()
        } else {
            Suffix::Type.into()
        },
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_symbol_name_handles_scip_clang() {
        assert_eq!(
            c_symbol_name("cxx . . $ foo_bar(134650f196314f84)."),
            Some("foo_bar".to_string())
        );
    }

    #[test]
    fn parses_gemspec_attrs() {
        let text = r#"
Gem::Specification.new do |s|
  s.name = "nokogiri"
  s.version = '1.16.0'
  s.summary = "HTML and XML parser"
end
"#;
        assert_eq!(
            extract_gemspec_attr(text, "name"),
            Some("nokogiri".to_string())
        );
        assert_eq!(
            extract_gemspec_attr(text, "version"),
            Some("1.16.0".to_string())
        );
    }

    #[test]
    fn gemspec_constant_version_yields_none() {
        let text = "s.version = Nokogiri::VERSION\n";
        assert_eq!(extract_gemspec_attr(text, "version"), None);
    }

    #[test]
    fn ruby_method_symbol_shape() {
        let s = ruby_symbol("mygem", "1.2.3", "Foo::Bar", "baz", true);
        assert_eq!(s, "scip-ruby gem mygem 1.2.3 Foo#Bar#baz().");
    }

    #[test]
    fn ruby_class_symbol_shape() {
        let s = ruby_symbol("mygem", "1.2.3", "Foo", "Bar", false);
        assert_eq!(s, "scip-ruby gem mygem 1.2.3 Foo#Bar#");
    }

    #[test]
    fn ruby_operator_method_is_escaped() {
        let s = ruby_symbol("mygem", "1.2.3", "Foo", "==", true);
        assert_eq!(s, "scip-ruby gem mygem 1.2.3 Foo#`==`().");
    }
}
