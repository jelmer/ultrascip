//! Rust source scanner for PyO3 exports.
//!
//! Walks the AST of each `.rs` file with `syn`, collecting:
//!
//!   * `#[pyfunction]` free functions
//!   * `#[pymodule]` module init functions
//!   * `#[pyclass]` structs / enums
//!   * `#[pymethods] impl` blocks and the `fn`s they contain
//!
//! For each export we record the Rust identifier (used to cross-reference the
//! Rust SCIP index) and the Python-visible name, which defaults to the Rust
//! name and is overridable via `#[pyo3(name = "...")]`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use syn::{Attribute, Expr, ExprLit, Item, ItemEnum, ItemFn, ItemImpl, ItemStruct, Lit, Meta};

#[derive(Debug, Clone)]
pub struct PyExport {
    /// SCIP-style relative path (`/` separators) from the source root.
    pub relative_path: PathBuf,
    /// Rust identifier as it appears in source (used to look up the Rust
    /// SCIP definition).
    pub rust_name: String,
    /// Python-visible name (defaults to `rust_name`; overridden by
    /// `#[pyo3(name = "...")]`).
    pub python_name: String,
    pub kind: PyExportKind,
    /// Zero-based `(line, start_col, end_col)` span of the exported name.
    pub name_span: (u32, u32, u32),
}

#[derive(Debug, Clone)]
pub enum PyExportKind {
    Module,
    Function,
    Class,
    Method { class: String },
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub exports: Vec<PyExport>,
}

pub fn scan_file(path: &Path, source_root: &Path, out: &mut ScanResult) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Cheap prefilter to skip files that clearly don't touch PyO3.
    if !text.contains("pyo3") && !text.contains("pyfunction") && !text.contains("pymodule") {
        return Ok(());
    }

    let file = match syn::parse_file(&text) {
        Ok(f) => f,
        Err(err) => {
            eprintln!("warning: parsing {}: {}", path.display(), err);
            return Ok(());
        }
    };

    let rel = path
        .strip_prefix(source_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());

    for item in &file.items {
        visit_item(item, &rel, None, out);
    }
    Ok(())
}

fn visit_item(item: &Item, rel: &Path, in_impl_ty: Option<&str>, out: &mut ScanResult) {
    match item {
        Item::Fn(f) => visit_fn(f, rel, in_impl_ty, out),
        Item::Struct(s) => visit_struct(s, rel, out),
        Item::Enum(e) => visit_enum(e, rel, out),
        Item::Impl(i) => visit_impl(i, rel, out),
        Item::Mod(m) => {
            if let Some((_, items)) = &m.content {
                for it in items {
                    visit_item(it, rel, in_impl_ty, out);
                }
            }
        }
        _ => {}
    }
}

fn visit_fn(f: &ItemFn, rel: &Path, in_impl_ty: Option<&str>, out: &mut ScanResult) {
    let rust_name = f.sig.ident.to_string();
    let name_span = span_of(&f.sig.ident.span(), rust_name.len());

    let mut kind: Option<PyExportKind> = None;
    let mut python_name = rust_name.clone();
    for attr in &f.attrs {
        if is_attr(attr, "pyfunction") {
            kind = Some(PyExportKind::Function);
        } else if is_attr(attr, "pymodule") {
            kind = Some(PyExportKind::Module);
        } else if is_attr(attr, "pyo3") {
            if let Some(name) = pyo3_name_override(attr) {
                python_name = name;
            }
        }
    }

    // Any fn inside a `#[pymethods] impl` block is a Python method by default.
    if kind.is_none() {
        if let Some(class) = in_impl_ty {
            kind = Some(PyExportKind::Method {
                class: class.to_string(),
            });
        }
    }

    if let Some(k) = kind {
        out.exports.push(PyExport {
            relative_path: rel.to_path_buf(),
            rust_name,
            python_name,
            kind: k,
            name_span,
        });
    }
}

fn visit_struct(s: &ItemStruct, rel: &Path, out: &mut ScanResult) {
    if !has_attr(&s.attrs, "pyclass") {
        return;
    }
    let python_name = pyclass_name_override(&s.attrs).unwrap_or_else(|| s.ident.to_string());
    let rust_name = s.ident.to_string();
    let name_span = span_of(&s.ident.span(), rust_name.len());
    out.exports.push(PyExport {
        relative_path: rel.to_path_buf(),
        rust_name,
        python_name,
        kind: PyExportKind::Class,
        name_span,
    });
}

fn visit_enum(e: &ItemEnum, rel: &Path, out: &mut ScanResult) {
    if !has_attr(&e.attrs, "pyclass") {
        return;
    }
    let python_name = pyclass_name_override(&e.attrs).unwrap_or_else(|| e.ident.to_string());
    let rust_name = e.ident.to_string();
    let name_span = span_of(&e.ident.span(), rust_name.len());
    out.exports.push(PyExport {
        relative_path: rel.to_path_buf(),
        rust_name,
        python_name,
        kind: PyExportKind::Class,
        name_span,
    });
}

fn visit_impl(i: &ItemImpl, rel: &Path, out: &mut ScanResult) {
    if !has_attr(&i.attrs, "pymethods") {
        return;
    }
    let class = match &*i.self_ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    };
    let Some(class) = class else { return };
    for item in &i.items {
        if let syn::ImplItem::Fn(m) = item {
            let rust_name = m.sig.ident.to_string();
            let name_span = span_of(&m.sig.ident.span(), rust_name.len());
            let mut python_name = rust_name.clone();
            for attr in &m.attrs {
                if is_attr(attr, "pyo3") {
                    if let Some(name) = pyo3_name_override(attr) {
                        python_name = name;
                    }
                }
            }
            out.exports.push(PyExport {
                relative_path: rel.to_path_buf(),
                rust_name,
                python_name,
                kind: PyExportKind::Method {
                    class: class.clone(),
                },
                name_span,
            });
        }
    }
}

fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| is_attr(a, name))
}

fn is_attr(attr: &Attribute, name: &str) -> bool {
    attr.path().is_ident(name)
}

/// Extract the string from `#[pyo3(name = "...")]`. Returns None for any other
/// pyo3 attribute shape.
fn pyo3_name_override(attr: &Attribute) -> Option<String> {
    let list = match &attr.meta {
        Meta::List(l) => l,
        _ => return None,
    };
    let mut result = None;
    let _ = list.parse_nested_meta(|meta| {
        if meta.path.is_ident("name") {
            let value: Expr = meta.value()?.parse()?;
            if let Expr::Lit(ExprLit {
                lit: Lit::Str(s), ..
            }) = value
            {
                result = Some(s.value());
            }
        }
        Ok(())
    });
    result
}

/// Look for `#[pyclass(name = "...")]` or `#[pyo3(name = "...")]` on the same
/// item.
fn pyclass_name_override(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !is_attr(attr, "pyclass") {
            continue;
        }
        if let Meta::List(list) = &attr.meta {
            let mut result: Option<String> = None;
            let _ = list.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let value: Expr = meta.value()?.parse()?;
                    if let Expr::Lit(ExprLit {
                        lit: Lit::Str(s), ..
                    }) = value
                    {
                        result = Some(s.value());
                    }
                }
                Ok(())
            });
            if result.is_some() {
                return result;
            }
        }
        for other in attrs {
            if is_attr(other, "pyo3") {
                if let Some(name) = pyo3_name_override(other) {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// Convert a proc_macro2 span into a `(line, start_col, end_col)` tuple in
/// SCIP's zero-based coordinates. proc_macro2 reports 1-based lines and
/// 0-based columns.
fn span_of(span: &proc_macro2::Span, name_len: usize) -> (u32, u32, u32) {
    let start = span.start();
    let line = start.line.saturating_sub(1) as u32;
    let col = start.column as u32;
    (line, col, col + name_len as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn scan_str(src: &str) -> ScanResult {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(src.as_bytes()).unwrap();
        let mut r = ScanResult::default();
        scan_file(f.path(), f.path().parent().unwrap(), &mut r).unwrap();
        r
    }

    #[test]
    fn finds_pyfunction() {
        let src = r#"
            use pyo3::prelude::*;
            #[pyfunction]
            fn _is_tree() -> bool { true }
        "#;
        let r = scan_str(src);
        assert_eq!(r.exports.len(), 1);
        assert_eq!(r.exports[0].rust_name, "_is_tree");
        assert_eq!(r.exports[0].python_name, "_is_tree");
        assert!(matches!(r.exports[0].kind, PyExportKind::Function));
    }

    #[test]
    fn finds_pymodule() {
        let src = r#"
            use pyo3::prelude::*;
            #[pymodule]
            fn _diff_tree(_py: pyo3::Python, m: &pyo3::Bound<pyo3::types::PyModule>) -> pyo3::PyResult<()> { Ok(()) }
        "#;
        let r = scan_str(src);
        assert_eq!(r.exports.len(), 1);
        assert_eq!(r.exports[0].rust_name, "_diff_tree");
        assert!(matches!(r.exports[0].kind, PyExportKind::Module));
    }

    #[test]
    fn honours_pyo3_name_override() {
        let src = r#"
            #[pyfunction]
            #[pyo3(name = "count_blocks")]
            fn _count_blocks() {}
        "#;
        let r = scan_str(src);
        assert_eq!(r.exports.len(), 1);
        assert_eq!(r.exports[0].rust_name, "_count_blocks");
        assert_eq!(r.exports[0].python_name, "count_blocks");
    }

    #[test]
    fn finds_pyclass_and_methods() {
        let src = r#"
            use pyo3::prelude::*;
            #[pyclass]
            struct Repo;
            #[pymethods]
            impl Repo {
                fn head(&self) -> String { String::new() }
            }
        "#;
        let r = scan_str(src);
        let kinds: Vec<_> = r.exports.iter().map(|e| (&e.rust_name, &e.kind)).collect();
        assert!(kinds
            .iter()
            .any(|(n, k)| n.as_str() == "Repo" && matches!(k, PyExportKind::Class)));
        assert!(kinds.iter().any(|(n, k)| n.as_str() == "head"
            && matches!(k, PyExportKind::Method { class } if class == "Repo")));
    }
}
