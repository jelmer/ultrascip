//! scip-c-abi-augment reads a Rust SCIP index (as produced by rust-analyzer)
//! and emits a companion SCIP index that adds C-side symbols for every Rust
//! function exposed under the C ABI: `pub extern "C" fn` and `pub extern fn`
//! (which defaults to `"C"`).
//!
//! rust-analyzer records the exact fn signature in each `SymbolInformation`'s
//! `signature_documentation.text`, and preserves the `extern "C"` in that
//! text even though it strips attributes like `#[no_mangle]`. That's enough
//! to identify every candidate export purely from the SCIP index -- including
//! functions synthesised by macros (e.g. `ffi_fn!` in `regex-capi`), which
//! source-level scanners miss because `syn` doesn't expand macros.
//!
//! For each such Rust fn we emit a C-side symbol with the scheme
//! `cxx . . $ <name>().` that scip-clang produces for a global-namespace C
//! function, so that a C indexer indexing a downstream consumer of the FFI
//! resolves the same string. Each emitted C symbol carries a `Relationship`
//! back to the Rust definition, so "find references" and "go to definition"
//! bridge the two indexes.
//!
//! Caveats:
//!
//! - `pub extern "C" fn` without `#[no_mangle]` / `#[export_name]` is still
//!   subject to Rust name mangling and is NOT a C ABI export. We can't tell
//!   from SCIP alone, since rust-analyzer strips attributes. In practice, in
//!   a `staticlib`/`cdylib` crate the two are near-synonymous; false positives
//!   just yield C symbols that nothing references.
//! - `#[export_name = "custom"]` renames the exported symbol. Not detectable
//!   from SCIP alone either.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use protobuf::Message;
use scip::types::{
    descriptor::Suffix, symbol_information::Kind, Descriptor, Document, Index, Metadata,
    Occurrence, Package, Relationship, Symbol, SymbolInformation, SymbolRole, ToolInfo,
};

/// Result of augmenting a Rust SCIP index with C-ABI companion symbols.
#[derive(Debug, Clone, Copy, Default)]
pub struct AugmentStats {
    pub documents: usize,
    pub exports: usize,
    pub external_symbols: usize,
}

/// Read the Rust SCIP index at `input`, produce a companion index with C ABI
/// symbols, and write it to `output`. Returns counts for logging.
pub fn augment_file(input: &Path, output: &Path) -> Result<AugmentStats> {
    let bytes =
        fs::read(input).with_context(|| format!("reading input SCIP {}", input.display()))?;
    let parsed: Index = Message::parse_from_bytes(&bytes)
        .with_context(|| format!("parsing input SCIP {}", input.display()))?;

    let exports = collect_exports(&parsed);
    let augmented = build_output(&parsed, &exports);

    let payload = augmented.write_to_bytes()?;
    fs::write(output, &payload)
        .with_context(|| format!("writing output SCIP {}", output.display()))?;

    Ok(AugmentStats {
        documents: augmented.documents.len(),
        exports: exports.len(),
        external_symbols: augmented.external_symbols.len(),
    })
}

/// One C-ABI export discovered in the input SCIP index.
#[derive(Debug, Clone)]
struct Export {
    /// The Rust SCIP symbol string of the exported fn. Used as the target of
    /// the Relationship on the emitted C symbol.
    rust_symbol: String,
    /// The C-facing export name. Rust-analyzer strips attributes, so we
    /// default to the DisplayName.
    export_name: String,
    /// The relative path of the Rust source file the export lives in, taken
    /// from the SCIP Document. Used to key the output Document.
    relative_path: String,
    /// (line, start_col, end_col) of the Definition occurrence if the input
    /// SCIP has one. `None` for macro-expanded fns where rust-analyzer didn't
    /// emit a source-range Definition.
    definition_range: Option<Vec<i32>>,
}

fn collect_exports(index: &Index) -> Vec<Export> {
    // Index Definition occurrences by symbol so we can attach a source range
    // to each export where the SCIP has one.
    let mut def_range_by_symbol: HashMap<&str, Vec<i32>> = HashMap::new();
    let mut file_of_symbol: HashMap<&str, &str> = HashMap::new();
    for doc in &index.documents {
        for occ in &doc.occurrences {
            if occ.symbol_roles & (SymbolRole::Definition as i32) != 0
                && !occ.symbol.starts_with("local ")
            {
                def_range_by_symbol.insert(&occ.symbol, occ.range.clone());
                file_of_symbol.insert(&occ.symbol, &doc.relative_path);
            }
        }
    }

    let mut exports = Vec::new();
    for doc in &index.documents {
        for si in &doc.symbols {
            if !is_c_abi_export(si) {
                continue;
            }
            if si.symbol.starts_with("local ") {
                continue;
            }
            let export_name = if !si.display_name.is_empty() {
                si.display_name.clone()
            } else if let Some(name) = terminal_descriptor_name(&si.symbol) {
                name
            } else {
                continue;
            };
            let definition_range = def_range_by_symbol.get(si.symbol.as_str()).cloned();
            // Prefer the file the Definition occurrence lives in when it
            // differs from the SymbolInformation's document (they usually
            // match, but macro-expanded fns can straddle).
            let relative_path = file_of_symbol
                .get(si.symbol.as_str())
                .copied()
                .unwrap_or(doc.relative_path.as_str())
                .to_string();
            exports.push(Export {
                rust_symbol: si.symbol.clone(),
                export_name,
                relative_path,
                definition_range,
            });
        }
    }
    // Deduplicate: rust-analyzer sometimes emits the same SymbolInformation
    // in multiple documents (e.g. `pub use` re-exports).
    exports.sort_by(|a, b| a.rust_symbol.cmp(&b.rust_symbol));
    exports.dedup_by(|a, b| a.rust_symbol == b.rust_symbol);
    exports
}

fn is_c_abi_export(si: &SymbolInformation) -> bool {
    // rust-analyzer strips attributes, so `#[no_mangle]` isn't visible here.
    // Instead we look at the signature text: it preserves `extern "C"` (and
    // the older attribute-less `extern` form, which also defaults to "C" for
    // fn ABIs).
    if si.kind.enum_value_or_default() != Kind::Function {
        return false;
    }
    let sig = si.signature_documentation.as_ref();
    let Some(sig) = sig else { return false };
    let text = &sig.text;
    // Only consider public fns. A `pub(crate)` or private fn can carry
    // `extern "C"` in tests but isn't a real ABI export.
    let text = match text.strip_prefix("pub ") {
        Some(rest) => rest,
        None => return false,
    };
    // Skip optional `unsafe`, `async`, `const` modifiers between `pub` and
    // `extern`. In practice `pub extern` and `pub unsafe extern` are the two
    // shapes we see.
    let text = strip_leading_word(text, "unsafe");
    let text = strip_leading_word(text, "async");
    let text = strip_leading_word(text, "const");
    text.starts_with("extern \"C\" fn ")
        || text.starts_with("extern \"C-unwind\" fn ")
        || text.starts_with("extern fn ")
}

fn strip_leading_word<'a>(s: &'a str, word: &str) -> &'a str {
    match s.strip_prefix(word) {
        Some(rest) if rest.starts_with(' ') => rest.trim_start(),
        _ => s,
    }
}

fn build_output(input: &Index, exports: &[Export]) -> Index {
    let mut out = Index::new();
    out.metadata = protobuf::MessageField::some(Metadata {
        version: input.metadata.version,
        tool_info: protobuf::MessageField::some(ToolInfo {
            name: "scip-c-abi-augment".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            arguments: vec![],
            special_fields: Default::default(),
        }),
        project_root: input.metadata.project_root.clone(),
        text_document_encoding: input.metadata.text_document_encoding,
        special_fields: Default::default(),
    });

    // Group exports by source file. Exports without a Definition range still
    // get grouped by file so their SymbolInformation stays adjacent to any
    // real ranges in the same doc; we just don't emit an Occurrence for them.
    let mut by_path: HashMap<String, Vec<&Export>> = HashMap::new();
    for e in exports {
        by_path.entry(e.relative_path.clone()).or_default().push(e);
    }

    for (path, exports) in by_path {
        let mut doc = Document::new();
        doc.language = "C".to_string();
        doc.relative_path = path;
        for export in exports {
            let symbol = c_function_symbol(&export.export_name);

            let mut si = SymbolInformation::new();
            si.symbol = symbol.clone();
            si.display_name = export.export_name.clone();
            si.kind = Kind::Function.into();
            let mut rel = Relationship::new();
            rel.symbol = export.rust_symbol.clone();
            rel.is_implementation = true;
            rel.is_reference = true;
            rel.is_definition = true;
            si.relationships.push(rel);
            doc.symbols.push(si);

            if let Some(range) = &export.definition_range {
                let mut occ = Occurrence::new();
                occ.symbol = symbol;
                occ.symbol_roles = SymbolRole::Definition as i32;
                occ.range = range.clone();
                doc.occurrences.push(occ);
            }
        }
        if !doc.symbols.is_empty() {
            out.documents.push(doc);
        }
    }

    out
}

/// Build the SCIP symbol string for a C-namespace function, mirroring
/// scip-clang's shape: `cxx . . $ <name>().`. Any downstream consumer that
/// indexes a C caller of this function will emit the same string as a
/// reference, so the augmenting index resolves it.
///
/// scip-clang uses `.` as both the placeholder for empty manager and package
/// name, and `$` as the version-slot placeholder for functions not attributed
/// to a versioned package. That's what we mirror here.
fn c_function_symbol(name: &str) -> String {
    let mut sym = Symbol::new();
    sym.scheme = "cxx".to_string();
    sym.package = protobuf::MessageField::some(Package {
        manager: ".".to_string(),
        name: ".".to_string(),
        version: "$".to_string(),
        special_fields: Default::default(),
    });
    sym.descriptors.push(Descriptor {
        name: name.to_string(),
        suffix: Suffix::Method.into(),
        ..Default::default()
    });
    scip::symbol::format_symbol(sym)
}

/// Extract the trailing descriptor name from a SCIP symbol string. Used as a
/// fallback when a SymbolInformation has no DisplayName.
fn terminal_descriptor_name(symbol: &str) -> Option<String> {
    let mut it = symbol.splitn(5, ' ');
    let _ = it.next()?;
    let _ = it.next()?;
    let _ = it.next()?;
    let _ = it.next()?;
    let descriptors = it.next()?;
    let mut last_name: Option<String> = None;
    let mut current = String::new();
    let mut chars = descriptors.chars();
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
                    // Skip disambiguator and closing paren.
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
                }
            }
            _ => current.push(c),
        }
    }
    last_name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_si_with_sig(sig_text: &str) -> SymbolInformation {
        let mut si = SymbolInformation::new();
        si.symbol = "rust-analyzer cargo x 1.0.0 mod/f().".to_string();
        si.display_name = "f".to_string();
        si.kind = Kind::Function.into();
        let mut sig = scip::types::Signature::new();
        sig.language = "rust".to_string();
        sig.text = sig_text.to_string();
        si.signature_documentation = protobuf::MessageField::some(sig);
        si
    }

    #[test]
    fn detects_pub_extern_c_fn() {
        let si = make_si_with_sig("pub extern \"C\" fn foo(x: i32)");
        assert!(is_c_abi_export(&si));
    }

    #[test]
    fn detects_pub_extern_fn_without_abi() {
        // `pub extern fn` defaults to `extern "C"` and is still a C-ABI fn.
        let si = make_si_with_sig("pub extern fn foo()");
        assert!(is_c_abi_export(&si));
    }

    #[test]
    fn detects_pub_unsafe_extern_c_fn() {
        let si = make_si_with_sig("pub unsafe extern \"C\" fn foo()");
        assert!(is_c_abi_export(&si));
    }

    #[test]
    fn detects_c_unwind_abi() {
        let si = make_si_with_sig("pub extern \"C-unwind\" fn foo()");
        assert!(is_c_abi_export(&si));
    }

    #[test]
    fn rejects_non_pub_extern_c_fn() {
        let si = make_si_with_sig("extern \"C\" fn foo()");
        assert!(!is_c_abi_export(&si));
    }

    #[test]
    fn rejects_pub_extern_rust_abi() {
        // `extern "Rust"` isn't a C ABI.
        let si = make_si_with_sig("pub extern \"Rust\" fn foo()");
        assert!(!is_c_abi_export(&si));
    }

    #[test]
    fn rejects_pub_extern_system_abi() {
        // `extern "system"` is Windows-only stdcall-alike; not the C ABI.
        let si = make_si_with_sig("pub extern \"system\" fn foo()");
        assert!(!is_c_abi_export(&si));
    }

    #[test]
    fn rejects_plain_rust_fn() {
        let si = make_si_with_sig("pub fn foo()");
        assert!(!is_c_abi_export(&si));
    }

    #[test]
    fn rejects_non_fn_kind() {
        let mut si = make_si_with_sig("pub extern \"C\" fn foo()");
        si.kind = Kind::Struct.into();
        assert!(!is_c_abi_export(&si));
    }

    #[test]
    fn c_function_symbol_shape() {
        // Matches scip-clang's `cxx . . $ <name>().` shape for a global-
        // namespace C function.
        let s = c_function_symbol("rure_free");
        assert_eq!(s, "cxx . . $ rure_free().");
    }

    #[test]
    fn terminal_descriptor_extracts_name() {
        assert_eq!(
            terminal_descriptor_name("rust-analyzer cargo rure 0.2.2 rure/rure_free()."),
            Some("rure_free".to_string())
        );
    }
}
