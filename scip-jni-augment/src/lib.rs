//! scip-jni-augment reads a C/C++ SCIP index (as produced by scip-clang) and
//! emits a companion SCIP index that adds Java-side symbols for every JNI
//! export found by scanning the sources with tree-sitter.
//!
//! The input SCIP gives us the exact symbol strings scip-clang assigned to
//! each C function (which we use as the target of the Java -> C relationship
//! links) and the project metadata to inherit. The Java-facing names come
//! from the exports themselves: the JNI name mangling embeds the fully
//! qualified class and method in the C identifier, and `RegisterNatives`
//! tables carry them as string literals.

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

mod jni_scan;

pub use jni_scan::demangle_jni;
use jni_scan::{JniExport, ScanResult};

/// Options for [`augment_file`].
#[derive(Debug, Default, Clone)]
pub struct AugmentOptions {
    /// Source tree to scan for C/C++ files. Defaults to the input index's
    /// `project_root` (or the current directory if none is set).
    pub source_root: Option<PathBuf>,
    /// Maven package name in the `maven/<groupId>/<artifactId>` form
    /// scip-java uses. Falls back to the coordinates in `pom.xml`, then to
    /// the unversioned-package placeholder.
    pub java_package: Option<String>,
    /// Maven package version. Falls back to `pom.xml`, then to the
    /// unversioned-package placeholder.
    pub java_version: Option<String>,
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
    /// The source tree contained no JNI exports; nothing was written.
    NoExports,
}

/// Read the C/C++ SCIP index at `input`, scan `opts.source_root` for JNI
/// exports, and write a companion Java-side SCIP index to `output`.
///
/// Returns [`AugmentOutcome::NoExports`] and does not write to `output` when
/// the scan finds no exports. Unlike the Python and Ruby augments there is
/// no skip for a missing package manifest: the mangled export names carry
/// the fully qualified Java names, so missing Maven coordinates only
/// degrade the package field to placeholders (mirroring scip-clang's own
/// unversioned-package handling).
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
        if let Err(err) = jni_scan::scan_file(entry.path(), &source_root, &mut scan) {
            eprintln!("warning: scanning {}: {}", entry.path().display(), err);
        }
    }

    if scan.exports.is_empty() {
        return Ok(AugmentOutcome::NoExports);
    }

    let pom = read_pom(&source_root);

    let java_package = opts
        .java_package
        .clone()
        .or_else(|| pom.as_ref().and_then(Pom::package_name))
        .unwrap_or_default();
    let java_version = opts
        .java_version
        .clone()
        .or_else(|| pom.as_ref().and_then(|p| p.version.clone()))
        .unwrap_or_default();

    let c_lookup = CSymbolLookup::from_index(&parsed);
    let augmented = build_output(&parsed, &java_package, &java_version, &scan, &c_lookup);

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
        .map(|s| matches!(s, "build" | "target") || s.starts_with('.'))
        .unwrap_or(false)
}

fn strip_file_scheme(root: &str) -> Option<PathBuf> {
    root.strip_prefix("file://").map(PathBuf::from)
}

#[derive(Debug, Default)]
struct Pom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
}

impl Pom {
    /// The scip-java package name form: `maven/<groupId>/<artifactId>`.
    fn package_name(&self) -> Option<String> {
        Some(format!(
            "maven/{}/{}",
            self.group_id.as_ref()?,
            self.artifact_id.as_ref()?
        ))
    }
}

/// Crude pom.xml coordinate extraction. The project's own coordinates are
/// preferred over the `<parent>` block's, but groupId and version fall back
/// to the parent's when the project inherits them.
fn read_pom(source_root: &Path) -> Option<Pom> {
    let text = fs::read_to_string(source_root.join("pom.xml")).ok()?;
    let own = strip_xml_block(&text, "parent");
    Some(Pom {
        group_id: extract_xml_tag(&own, "groupId").or_else(|| extract_xml_tag(&text, "groupId")),
        artifact_id: extract_xml_tag(&own, "artifactId"),
        version: extract_xml_tag(&own, "version").or_else(|| extract_xml_tag(&text, "version")),
    })
}

fn strip_xml_block(text: &str, tag: &str) -> String {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    match (text.find(&open), text.find(&close)) {
        (Some(start), Some(end)) if end > start => {
            format!("{}{}", &text[..start], &text[end + close.len()..])
        }
        _ => text.to_string(),
    }
}

fn extract_xml_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let value = text[start..end].trim();
    (!value.is_empty() && !value.contains('<')).then(|| value.to_string())
}

/// Lookup for C SCIP symbols by their identifier (e.g.
/// "Java_com_example_Foo_bar"), used to link each Java symbol we emit to the
/// C definition it wraps.
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
    java_package: &str,
    java_version: &str,
    scan: &ScanResult,
    c_lookup: &CSymbolLookup,
) -> Index {
    let mut out = Index::new();
    out.metadata = protobuf::MessageField::some(Metadata {
        version: input.metadata.version,
        tool_info: protobuf::MessageField::some(ToolInfo {
            name: "scip-jni-augment".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arguments: vec![],
            special_fields: Default::default(),
        }),
        project_root: input.metadata.project_root.clone(),
        text_document_encoding: input.metadata.text_document_encoding,
        special_fields: Default::default(),
    });

    let mut by_path: HashMap<PathBuf, Vec<JniExport>> = HashMap::new();
    for export in &scan.exports {
        by_path
            .entry(export.relative_path.clone())
            .or_default()
            .push(export.clone());
    }

    for (relpath, exports) in by_path {
        let mut doc = Document::new();
        doc.language = "Java".to_string();
        doc.relative_path = relpath.to_string_lossy().into_owned();

        for export in &exports {
            emit_export(&mut doc, export, java_package, java_version, c_lookup);
        }

        if !doc.symbols.is_empty() {
            out.documents.push(doc);
        }
    }

    out
}

fn emit_export(
    doc: &mut Document,
    export: &JniExport,
    java_package: &str,
    java_version: &str,
    c_lookup: &CSymbolLookup,
) {
    let symbol = java_method_symbol(java_package, java_version, &export.class, &export.java_name);

    let mut si = SymbolInformation::new();
    si.symbol = symbol.clone();
    si.display_name = export.java_name.clone();
    si.kind = Kind::Method.into();
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

/// Build a scip-java-style symbol: package components are Namespace
/// descriptors (`com/example/`), the class (and any `$`-nested classes) Type
/// descriptors (`Foo#`), the method a Method descriptor (`bar().`). Empty
/// package name or version format as the `.` placeholder, like scip-clang's
/// unversioned packages.
fn java_method_symbol(package: &str, version: &str, class: &[String], method: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "semanticdb".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: "maven".to_string(),
        name: package.to_string(),
        version: version.to_string(),
        special_fields: Default::default(),
    });
    let (class_name, package_parts) = match class.split_last() {
        Some((last, rest)) => (last.as_str(), rest),
        None => return String::new(),
    };
    for part in package_parts {
        sym.descriptors.push(Descriptor {
            name: part.to_string(),
            suffix: Suffix::Namespace.into(),
            ..Default::default()
        });
    }
    for part in class_name.split('$').filter(|s| !s.is_empty()) {
        sym.descriptors.push(Descriptor {
            name: part.to_string(),
            suffix: Suffix::Type.into(),
            ..Default::default()
        });
    }
    sym.descriptors.push(Descriptor {
        name: method.to_string(),
        suffix: Suffix::Method.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_symbol_shape() {
        let s = java_method_symbol(
            "maven/com.example/calc",
            "1.0.0",
            &["com".into(), "example".into(), "Calc".into()],
            "add",
        );
        assert_eq!(
            s,
            "semanticdb maven maven/com.example/calc 1.0.0 com/example/Calc#add()."
        );
    }

    #[test]
    fn java_symbol_placeholder_package() {
        let s = java_method_symbol("", "", &["Foo".into()], "bar");
        assert_eq!(s, "semanticdb maven . . Foo#bar().");
    }

    #[test]
    fn java_symbol_nested_class() {
        let s = java_method_symbol("", "", &["com".into(), "Foo$Inner".into()], "bar");
        assert_eq!(s, "semanticdb maven . . com/Foo#Inner#bar().");
    }

    #[test]
    fn parses_pom_coordinates() {
        let text = r#"
<project>
  <parent>
    <groupId>org.parent</groupId>
    <artifactId>parent-pom</artifactId>
    <version>7</version>
  </parent>
  <groupId>com.example</groupId>
  <artifactId>calc</artifactId>
  <version>1.0.0</version>
</project>
"#;
        let own = strip_xml_block(text, "parent");
        assert_eq!(
            extract_xml_tag(&own, "groupId"),
            Some("com.example".to_string())
        );
        assert_eq!(
            extract_xml_tag(&own, "artifactId"),
            Some("calc".to_string())
        );
        assert_eq!(extract_xml_tag(&own, "version"), Some("1.0.0".to_string()));
    }

    #[test]
    fn pom_inherits_group_and_version_from_parent() {
        let text = r#"
<project>
  <parent>
    <groupId>org.parent</groupId>
    <artifactId>parent-pom</artifactId>
    <version>7</version>
  </parent>
  <artifactId>calc</artifactId>
</project>
"#;
        let own = strip_xml_block(text, "parent");
        assert_eq!(extract_xml_tag(&own, "groupId"), None);
        assert_eq!(
            extract_xml_tag(text, "groupId"),
            Some("org.parent".to_string())
        );
    }
}
